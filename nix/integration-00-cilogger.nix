{
  pkgs,
  cilogger,
  ...
}:
pkgs.testers.runNixOSTest {
  name = "cilogger-integration";

  globalTimeout = 5 * 60;

  nodes.machine = _: {
    environment.systemPackages = [
      cilogger
      pkgs.procps
    ];
  };

  testScript = ''
    import time

    start_all()

    machine.succeed("cilogger --version")

    # Normal exit from the "command" ran.
    out = machine.succeed(
        "cilogger -- /bin/sh -c 'for i in $(seq 1 10); do echo line-$i; done'"
    )
    assert out == "", f"expected no stdout output, got: {out!r}"

    machine.fail("cilogger -- /bin/sh -c 'echo boom; exit 42'")

    # note sh in nixos tests runs with -e so have to keep the shell here -e compatible.
    last_line = machine.succeed(
        "cilogger -- /bin/sh -c 'echo kaboom; exit 42' && rc=0 || rc=$?; echo \"rc=$rc\""
    ).strip().splitlines()[-1]
    assert last_line == "rc=42", f"expected exit code 42 got: {last_line} instead"

    # Validate the cilogger output is what we want.
    machine.succeed("rm -f /tmp/cilogger.log")
    out = machine.succeed(
        "cilogger -l /tmp/cilogger.log -- "
        "/bin/sh -c 'for i in $(seq 1 20); do echo line-$i; done'"
    )
    assert out == "", f"expected no stdout output with --log-file got: {out!r} instead"
    full_log = machine.succeed("cat /tmp/cilogger.log").strip().splitlines()
    expected_full = [f"line-{i}" for i in range(1, 21)]
    assert full_log == expected_full, f"log file missing lines: {full_log}"

    machine.succeed("rm -f /tmp/cilogger-ts.log")
    out = machine.succeed(
        "cilogger -T -l /tmp/cilogger-ts.log -- /bin/sh -c 'echo one; echo two'"
    )
    assert out == "", f"expected no stdout output, got: {out!r}"
    ts_lines = machine.succeed("cat /tmp/cilogger-ts.log").strip().splitlines()
    assert len(ts_lines) == 2, f"unexpected line count: {ts_lines}"
    for line, expected_text in zip(ts_lines, ["one", "two"]):
        prefix, _, text = line.partition(" ")
        assert text == expected_text, f"unexpected line content: {line}"
        assert "T" in prefix and prefix.endswith("Z"), f"missing timestamp prefix: {line}"

    machine.succeed("rm -f /tmp/cilogger-repeat.count")
    repeat_script = (
        "n=$(cat /tmp/cilogger-repeat.count 2>/dev/null || echo 0); "
        "n=$((n + 1)); echo $n > /tmp/cilogger-repeat.count; "
        '[ "$n" -lt 3 ] && exit 0; exit 9'
    )
    last_line = machine.succeed(
        f"cilogger -r -- /bin/sh -c '{repeat_script}' && rc=0 || rc=$?; echo \"rc=$rc\""
    ).strip().splitlines()[-1]
    assert last_line == "rc=9", f"expected exit code 9 got: {last_line} instead"
    repeat_runs = int(machine.succeed("cat /tmp/cilogger-repeat.count").strip())
    assert repeat_runs == 3, f"expected exactly 3 invocations under --repeat, got {repeat_runs}"

    # --repeat with --timeout is a bit different.
    machine.succeed("rm -f /tmp/cilogger-repeat-timeout.count")
    timeout_script = (
        "n=$(cat /tmp/cilogger-repeat-timeout.count 2>/dev/null || echo 0); "
        "echo $((n + 1)) > /tmp/cilogger-repeat-timeout.count; sleep 0.05"
    )
    machine.succeed(f"cilogger -r -t 500ms -- /bin/sh -c '{timeout_script}'")
    repeat_timeout_runs = int(
        machine.succeed("cat /tmp/cilogger-repeat-timeout.count").strip()
    )
    assert repeat_timeout_runs > 1, (
        f"expected more than one iteration under --repeat --timeout, "
        f"got {repeat_timeout_runs}"
    )

    # --timeout kill of the process group
    machine.succeed(
        "cilogger -t 300ms -- /bin/sh -c 'sleep 20 & echo spawned $!; wait'"
    )
    machine.sleep(1)
    machine.fail("pgrep -x sleep")

    # Test out all the signals cilogger should handle.
    def assert_signal_stops_gracefully(sig):
        unit = f"cilogger-sig-{sig.lower()}"
        machine.succeed(
            f"systemd-run --unit={unit} "
            "--property=KillMode=none --property=Type=exec "
            "--setenv=PATH=/run/wrappers/bin:/run/current-system/sw/bin:/bin:/usr/bin:/sbin:/usr/sbin "
            "-- cilogger -- /bin/sh -c 'sleep 20 & wait'"
        )
        machine.wait_until_succeeds("pgrep -x sleep", timeout=10)
        machine.succeed(f"systemctl kill --kill-who=main --signal=SIG{sig} {unit}.service")

        # Poll the hard way, the nix test suites a bit weird.
        deadline = time.monotonic() + 15
        last_ps = None
        while time.monotonic() < deadline:
            status, _ = machine.execute("pgrep -x sleep")
            if status != 0:
                break
            last_ps = machine.succeed(
                "COLUMNS=500 ps -eo pid,ppid,pgid,sid,stat,cmd --sort=pid"
            )
            time.sleep(1)
        else:
            print(machine.succeed(f"journalctl -u {unit}.service --no-pager"))
            print(last_ps)
            raise Exception(f"'sleep' is still running 15s after SIG{sig} to cilogger was sent")

        exit_status = machine.succeed(
            f"systemctl show {unit}.service -p ExecMainStatus --value"
        ).strip()
        assert exit_status == "0", f"cilogger should exit 0 with SIG{sig} got {exit_status} instead"

    for sig in ["HUP", "INT", "QUIT", "TERM", "USR1", "USR2"]:
        assert_signal_stops_gracefully(sig)

    print("cilogger integration test passed probably works.")
  '';
}
