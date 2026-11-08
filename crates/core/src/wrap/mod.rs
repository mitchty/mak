mod mmap_log;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use mmap_log::MmapLog;
use nix::{
    sys::signal::{self, Signal as NixSignal},
    unistd::Pid,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::{Child, Command},
    signal::unix::{Signal as UnixSignal, SignalKind, signal as unix_signal},
    sync::Mutex,
    time::Instant,
};

/// How often we force a mmap flush time wise. 150ms should be a good amount.
const FLUSH_INTERVAL: Duration = Duration::from_millis(150);

/// How long we wait until we send KILL to the group after TERM
const KILL_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct WrapConfig {
    /// Command for exec() as a vector of Strings.
    pub command: Vec<String>,
    /// Optional path to the log file.
    pub log_file: Option<PathBuf>,
    /// Optional timeout for the whole run.
    pub timeout: Option<Duration>,
    /// Prefix the log lines with rfc3339 timestamps for things that want it.
    pub timestamps: bool,
    /// Repeat the command until it fails or we timeout or not.
    pub repeat: bool,
    /// Shell command that must succeed before `command` is looped. Iff it fails
    /// its failure is printed instead of running the main command loop.
    pub prime: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It failed with this return code
    Exited(i32),
    /// We hit timeout.
    TimedOut,
    /// We got signalled probably by the ci runner instead.
    Signaled(WrapSignal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapSignal {
    /// `SIGHUP`
    Hup,
    /// `SIGINT`
    Int,
    /// `SIGQUIT`
    Quit,
    /// `SIGTERM`
    Term,
    /// `SIGUSR1`
    Usr1,
    /// `SIGUSR2`
    Usr2,
}

impl std::fmt::Display for WrapSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            WrapSignal::Hup => "SIGHUP",
            WrapSignal::Int => "SIGINT",
            WrapSignal::Quit => "SIGQUIT",
            WrapSignal::Term => "SIGTERM",
            WrapSignal::Usr1 => "SIGUSR1",
            WrapSignal::Usr2 => "SIGUSR2",
        };
        f.write_str(name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WrapError {
    /// Mitch forgot to pass a command like a dum dum
    #[error("no command for exec()")]
    EmptyCommand,
    /// Mitch also forgot to pass in something to --prime too or did "$foo" without $foo set
    #[error("no prime command for exec()")]
    EmptyPrime,
    /// Couldn't exec() whatever it was.
    #[error("failed to exec command: {0}")]
    Spawn(#[source] std::io::Error),
    /// No child process spawned to wait on, weird edge case.
    #[error("failed to wait on child process: {0}")]
    Wait(#[source] std::io::Error),
    /// Signal handler failed to install.
    #[error("failed to install the signal handler: {0}")]
    SignalSetup(#[source] std::io::Error),
    /// Log file had an aneurysm writing.
    #[error("failed to write the log file: {0}")]
    LogFile(#[source] std::io::Error),
}

/// Just handle a the timeout/signal setup versus the child exited.
#[must_use]
pub fn process_exit_code(outcome: Outcome) -> i32 {
    match outcome {
        Outcome::Exited(code) => code,
        Outcome::TimedOut | Outcome::Signaled(_) => 0,
    }
}

/// Simple stats for what was ran and how many times and its duration.
#[derive(Debug, Default, Clone, Copy)]
struct RunStats {
    runs: u32,
    total: Duration,
}

impl RunStats {
    fn record(&mut self, elapsed: Duration) {
        self.runs += 1;
        self.total += elapsed;
    }

    fn average(&self) -> Duration {
        if self.runs == 0 {
            Duration::ZERO
        } else {
            self.total / self.runs
        }
    }
}

impl std::fmt::Display for RunStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} run{} average {} per run",
            self.runs,
            if self.runs == 1 { "" } else { "s" },
            humantime::format_duration(self.average())
        )
    }
}

pub async fn run(config: WrapConfig) -> Result<Outcome, WrapError> {
    let Some((program, args)) = config.command.split_first() else {
        return Err(WrapError::EmptyCommand);
    };

    // The prime command is passed through sh -c so callers can do 'foo | bar' ezpzlemonsqeezy in scripts.
    let prime_args = match config.prime.as_deref().map(str::trim) {
        Some("") => return Err(WrapError::EmptyPrime),
        Some(prime) => Some(["-c".to_string(), prime.to_string()]),
        None => None,
    };

    let state = Arc::new(Mutex::new(SharedState::new(
        config.log_file.as_deref(),
        config.timestamps,
    )?));
    let flush_task = tokio::spawn(periodic_flush(Arc::clone(&state)));

    let mut signals = Signals::install()?;
    let deadline = config.timeout.map(|d| Instant::now() + d);

    // The prime command must succeed before the command loop is ran.
    let primed = match prime_args {
        Some(args) => {
            let (outcome, _) = run_child("sh", &args, &state, &mut signals, deadline).await?;
            match outcome {
                Outcome::Exited(0) => None,
                Outcome::Exited(code) => {
                    eprintln!("cilogger: prime command failed, exit code {code}");
                    Some(outcome)
                }
                Outcome::TimedOut => {
                    eprintln!("cilogger: timeout elapsed during prime command");
                    Some(outcome)
                }
                Outcome::Signaled(sig) => {
                    eprintln!("cilogger: received {sig} during prime command");
                    Some(outcome)
                }
            }
        }
        None => None,
    };

    let mut stats = RunStats::default();
    let outcome = match primed {
        Some(outcome) => outcome,
        None => loop {
            let (outcome, elapsed) =
                run_child(program, args, &state, &mut signals, deadline).await?;
            if let Outcome::Exited(_) = outcome {
                stats.record(elapsed);
            }
            if config.repeat && outcome == Outcome::Exited(0) {
                continue;
            }
            break outcome;
        },
    };

    // Dump out the exec() stats just not to the log file.
    match outcome {
        _ if primed.is_some() => {}
        Outcome::Signaled(sig) if config.repeat => {
            eprintln!("cilogger: received {sig} after {stats}, stopping wrapped command");
        }
        Outcome::Signaled(sig) => eprintln!("cilogger: received {sig}, stopping wrapped command"),
        Outcome::TimedOut if config.repeat => {
            eprintln!("cilogger: timeout elapsed after {stats}, stopping wrapped command");
        }
        Outcome::TimedOut => eprintln!("cilogger: timeout elapsed, stopping wrapped command"),
        Outcome::Exited(code) if config.repeat && code != 0 => {
            eprintln!("cilogger: command failed after {stats}, exit code {code}");
        }
        Outcome::Exited(_) => {}
    }

    flush_task.abort();
    state.lock().await.finish()?;

    Ok(outcome)
}

/// Signals cilogger wraps instead of letting the underlying os handle. This is
/// mostly so that the child group can be stopped cleanly and we exit 0 in the
/// event of things like a timeout.
struct Signals {
    hup: UnixSignal,
    int: UnixSignal,
    quit: UnixSignal,
    term: UnixSignal,
    usr1: UnixSignal,
    usr2: UnixSignal,
}

impl Signals {
    fn install() -> Result<Self, WrapError> {
        let install = |kind| unix_signal(kind).map_err(WrapError::SignalSetup);
        Ok(Self {
            hup: install(SignalKind::hangup())?,
            int: install(SignalKind::interrupt())?,
            quit: install(SignalKind::quit())?,
            term: install(SignalKind::terminate())?,
            usr1: install(SignalKind::user_defined1())?,
            usr2: install(SignalKind::user_defined2())?,
        })
    }

    async fn recv(&mut self) -> WrapSignal {
        tokio::select! {
            _ = self.hup.recv() => WrapSignal::Hup,
            _ = self.int.recv() => WrapSignal::Int,
            _ = self.quit.recv() => WrapSignal::Quit,
            _ = self.term.recv() => WrapSignal::Term,
            _ = self.usr1.recv() => WrapSignal::Usr1,
            _ = self.usr2.recv() => WrapSignal::Usr2,
        }
    }
}

/// Run one child process to completion, a signal or deadline, stopping the
/// process group if the child hasn't exited yet. Returns the outcome and how
/// long it ran for logging purposes.
async fn run_child(
    program: &str,
    args: &[String],
    state: &Arc<Mutex<SharedState>>,
    signals: &mut Signals,
    deadline: Option<Instant>,
) -> Result<(Outcome, Duration), WrapError> {
    let mut child = spawn(program, args)?;
    let pid = child.id().map(|id| id as i32);
    let stdout = child.stdout.take().expect("stdout was piped at spawn");
    let stderr = child.stderr.take().expect("stderr was piped at spawn");

    let stdout_task = tokio::spawn(pump_lines(stdout, Arc::clone(state)));
    let stderr_task = tokio::spawn(pump_lines(stderr, Arc::clone(state)));

    let started = Instant::now();
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(exit_code(status.map_err(WrapError::Wait)?)),
        sig = signals.recv() => Outcome::Signaled(sig),
        () = deadline_or_pending(deadline) => Outcome::TimedOut,
    };
    let elapsed = started.elapsed();

    let stop_pid = pid.filter(|_| !matches!(outcome, Outcome::Exited(_)));
    if let Some(pid) = stop_pid {
        stop_process_group(pid, &mut child).await;
    }

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    Ok((outcome, elapsed))
}

fn spawn(program: &str, args: &[String]) -> Result<Child, WrapError> {
    Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(WrapError::Spawn)
}

async fn stop_process_group(pid: i32, child: &mut Child) {
    let group = Pid::from_raw(-pid);
    let _ = signal::kill(group, NixSignal::SIGTERM);

    if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_ok() {
        return;
    }

    let _ = signal::kill(group, NixSignal::SIGKILL);
    let _ = child.wait().await;
}

async fn deadline_or_pending(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

async fn pump_lines<R>(reader: R, state: Arc<Mutex<SharedState>>)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        state.lock().await.push_line(line);
    }
}

async fn periodic_flush(state: Arc<Mutex<SharedState>>) {
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    loop {
        ticker.tick().await;
        if let Err(err) = state.lock().await.flush_pending() {
            log::warn!("cilogger: failed to flush log file: {err}");
        }
    }
}

struct SharedState {
    log: Option<MmapLog>,
    pending: Vec<u8>,
    timestamps: bool,
}

impl SharedState {
    fn new(log_file: Option<&std::path::Path>, timestamps: bool) -> Result<Self, WrapError> {
        let log = log_file
            .map(MmapLog::create)
            .transpose()
            .map_err(WrapError::LogFile)?;
        Ok(Self {
            log,
            pending: Vec::new(),
            timestamps,
        })
    }

    /// We only dump out full lines, partial lines are ignored until they have a \n or \r.
    fn push_line(&mut self, line: String) {
        if self.log.is_none() {
            return;
        }
        let line = if self.timestamps {
            format!(
                "{} {line}",
                humantime::format_rfc3339_millis(SystemTime::now())
            )
        } else {
            line
        };
        self.pending.extend_from_slice(line.as_bytes());
        self.pending.push(b'\n');
    }

    fn flush_pending(&mut self) -> std::io::Result<()> {
        let Some(log) = &mut self.log else {
            return Ok(());
        };
        if !self.pending.is_empty() {
            log.append(&self.pending)?;
            self.pending.clear();
        }
        log.sync()
    }

    fn finish(&mut self) -> Result<(), WrapError> {
        self.flush_pending().map_err(WrapError::LogFile)?;
        if let Some(log) = self.log.take() {
            log.finish().map_err(WrapError::LogFile)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config(command: Vec<String>) -> WrapConfig {
        WrapConfig {
            command,
            log_file: None,
            timeout: None,
            timestamps: false,
            repeat: false,
            prime: None,
        }
    }

    fn counter_path(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cilogger-prime-test-{tag}-{}.count",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn tick(path: &std::path::Path) -> String {
        format!("echo x >> {}", path.display())
    }

    fn ticks(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |s| s.lines().count())
    }

    #[tokio::test]
    async fn prime_success_runs_before_the_command() {
        let order = counter_path("order");
        let config = WrapConfig {
            prime: Some(format!("echo prime >> {}", order.display())),
            ..base_config(vec![
                "sh".into(),
                "-c".into(),
                format!("echo main >> {}", order.display()),
            ])
        };

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::Exited(0));
        let contents = std::fs::read_to_string(&order).expect("read order file");
        assert_eq!(contents.lines().collect::<Vec<_>>(), ["prime", "main"]);
        let _ = std::fs::remove_file(&order);
    }

    #[tokio::test]
    async fn prime_failure_skips_the_command_and_propagates_its_code() {
        let main_ran = counter_path("skip");
        let config = WrapConfig {
            prime: Some("exit 5".into()),
            repeat: true,
            ..base_config(vec!["sh".into(), "-c".into(), tick(&main_ran)])
        };

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::Exited(5));
        assert_eq!(
            ticks(&main_ran),
            0,
            "command must not run after failed prime"
        );
    }

    #[tokio::test]
    async fn prime_runs_once_even_when_repeating() {
        let prime_runs = counter_path("once-prime");
        let main_runs = counter_path("once-main");
        let script = format!(
            "{}; [ \"$(wc -l < {})\" -lt 3 ] && exit 0; exit 9",
            tick(&main_runs),
            main_runs.display()
        );
        let config = WrapConfig {
            prime: Some(tick(&prime_runs)),
            repeat: true,
            ..base_config(vec!["sh".into(), "-c".into(), script])
        };

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::Exited(9));
        assert_eq!(ticks(&prime_runs), 1);
        assert_eq!(ticks(&main_runs), 3);
        let _ = std::fs::remove_file(&prime_runs);
        let _ = std::fs::remove_file(&main_runs);
    }

    #[tokio::test]
    async fn timeout_during_prime_skips_the_command_and_is_success() {
        let main_ran = counter_path("timeout");
        let config = WrapConfig {
            prime: Some("sleep 5".into()),
            timeout: Some(Duration::from_millis(100)),
            ..base_config(vec!["sh".into(), "-c".into(), tick(&main_ran)])
        };

        let started = std::time::Instant::now();
        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::TimedOut);
        assert_eq!(process_exit_code(outcome), 0);
        assert_eq!(ticks(&main_ran), 0);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn empty_prime_is_rejected() {
        let config = WrapConfig {
            prime: Some("  ".into()),
            ..base_config(vec!["true".into()])
        };

        assert!(matches!(run(config).await, Err(WrapError::EmptyPrime)));
    }

    #[tokio::test]
    async fn normal_exit_reports_real_exit_code() {
        let config = base_config(vec![
            "sh".into(),
            "-c".into(),
            "echo hello; echo world >&2".into(),
        ]);

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::Exited(0));
        assert_eq!(process_exit_code(outcome), 0);
    }

    #[tokio::test]
    async fn failing_command_propagates_its_exit_code() {
        let config = base_config(vec!["sh".into(), "-c".into(), "exit 7".into()]);

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::Exited(7));
        assert_eq!(process_exit_code(outcome), 7);
    }

    #[tokio::test]
    async fn timeout_kills_the_command_and_is_reported_as_success() {
        let config = WrapConfig {
            timeout: Some(Duration::from_millis(100)),
            ..base_config(vec!["sh".into(), "-c".into(), "sleep 5".into()])
        };

        let started = std::time::Instant::now();
        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::TimedOut);
        assert_eq!(process_exit_code(outcome), 0);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn empty_command_is_rejected() {
        let config = base_config(vec![]);

        assert!(matches!(run(config).await, Err(WrapError::EmptyCommand)));
    }

    #[tokio::test]
    async fn log_file_captures_full_output() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cilogger-wrap-test-{}.log", std::process::id()));

        let config = WrapConfig {
            log_file: Some(path.clone()),
            ..base_config(vec![
                "sh".into(),
                "-c".into(),
                "echo hello; echo world >&2".into(),
            ])
        };

        let outcome = run(config).await.expect("run succeeds");
        assert_eq!(outcome, Outcome::Exited(0));

        let contents = std::fs::read_to_string(&path).expect("read log file");
        let lines: Vec<&str> = contents.lines().collect();
        assert!(lines.contains(&"hello"), "log missing 'hello': {lines:?}");
        assert!(lines.contains(&"world"), "log missing 'world': {lines:?}");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn timestamps_prefix_each_complete_line_in_the_log_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cilogger-wrap-test-ts-{}.log", std::process::id()));

        let config = WrapConfig {
            log_file: Some(path.clone()),
            timestamps: true,
            ..base_config(vec!["sh".into(), "-c".into(), "echo hello".into()])
        };

        run(config).await.expect("run succeeds");

        let contents = std::fs::read_to_string(&path).expect("read log file");
        let line = contents.lines().next().expect("log file has one line");
        let (prefix, rest) = line
            .split_once(' ')
            .expect("timestamp and content separated by a space");
        assert_eq!(rest, "hello");
        assert!(
            humantime::parse_rfc3339(prefix).is_ok(),
            "expected an rfc3339 timestamp prefix, got: {prefix}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn repeat_stops_on_first_nonzero_exit_after_looping() {
        let dir = std::env::temp_dir();
        let counter = dir.join(format!("cilogger-repeat-test-{}.count", std::process::id()));
        let _ = std::fs::remove_file(&counter);

        let script = format!(
            "n=$(cat {path} 2>/dev/null || echo 0); n=$((n + 1)); echo $n > {path}; \
             [ \"$n\" -lt 3 ] && exit 0; exit 9",
            path = counter.display()
        );

        let config = WrapConfig {
            repeat: true,
            ..base_config(vec!["sh".into(), "-c".into(), script])
        };

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::Exited(9));
        let runs: u32 = std::fs::read_to_string(&counter)
            .expect("read counter")
            .trim()
            .parse()
            .expect("counter is a number");
        assert_eq!(runs, 3, "expected exactly 3 invocations before failing");

        let _ = std::fs::remove_file(&counter);
    }

    #[tokio::test]
    async fn repeat_with_timeout_stops_mid_loop_and_is_reported_as_success() {
        let dir = std::env::temp_dir();
        let counter = dir.join(format!(
            "cilogger-repeat-timeout-test-{}.count",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&counter);

        let script = format!(
            "n=$(cat {path} 2>/dev/null || echo 0); echo $((n + 1)) > {path}; sleep 0.05",
            path = counter.display()
        );

        let config = WrapConfig {
            timeout: Some(Duration::from_millis(250)),
            repeat: true,
            ..base_config(vec!["sh".into(), "-c".into(), script])
        };

        let outcome = run(config).await.expect("run succeeds");

        assert_eq!(outcome, Outcome::TimedOut);
        assert_eq!(process_exit_code(outcome), 0);
        let runs: u32 = std::fs::read_to_string(&counter)
            .expect("read counter")
            .trim()
            .parse()
            .expect("counter is a number");
        assert!(
            runs > 1,
            "expected more than one iteration but got {runs} instead"
        );

        let _ = std::fs::remove_file(&counter);
    }
}
