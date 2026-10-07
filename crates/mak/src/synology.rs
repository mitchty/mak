use std::collections::HashMap;

use bytesize::ByteSize;
use clap::Subcommand;
use color_eyre::eyre::{Result, WrapErr, bail};
use reqwest::Client as HttpClient;
use serde_json::Value;
use tabled::settings::object::Columns;
use tabled::settings::{Alignment, Style};
use tabled::{Table, Tabled};

/// `mak synology ...` subcommands.
#[derive(Subcommand)]
pub enum SynologyCommand {
    /// Synology iSCSI LUN/Target management
    Iscsi {
        #[command(subcommand)]
        command: IscsiCommand,
    },
}

/// LUN allocation strategy maps to the Synology API `type` field.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LunAllocation {
    /// Thin-provisioned `BLUN`: default.
    #[default]
    Thin,
    /// Thick-provisioned `BLUN_THICK`.
    Thick,
}

impl LunAllocation {
    fn as_api_type(self) -> &'static str {
        match self {
            LunAllocation::Thin => "BLUN",
            LunAllocation::Thick => "BLUN_THICK",
        }
    }
}

impl std::fmt::Display for LunAllocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LunAllocation::Thin => "thin",
            LunAllocation::Thick => "thick",
        })
    }
}

// Common subcommand args for synology iscsi commands
#[derive(clap::Args)]
pub struct SynologyConnection {
    /// Synology hostname or IP
    #[arg(long, env = "SYNOLOGY_HOST")]
    host: String,

    /// Synology port
    #[arg(long, env = "SYNOLOGY_PORT", default_value = "5000")]
    port: u16,

    /// Username
    #[arg(short = 'u', long, env = "SYNOLOGY_USER")]
    user: String,

    /// Password
    #[arg(short = 'P', long, env = "SYNOLOGY_PASS")]
    password: String,

    /// Use HTTPS instead of HTTP
    #[arg(short = 's', long, env = "SYNOLOGY_USE_HTTPS")]
    https: bool,
}

#[derive(Subcommand)]
pub enum IscsiCommand {
    /// Create a LUN and Target
    Create {
        #[command(flatten)]
        conn: SynologyConnection,

        /// Name for the LUN and Target used as both LUN name and as part of the IQN
        #[arg(long)]
        name: String,

        /// LUN size, in human-readable units e.g. 10g, 20GiB, 500MB
        #[arg(long)]
        size: ByteSize,

        /// LUN allocation strategy: thin default, or thick
        #[arg(long, default_value = "thin")]
        allocation: LunAllocation,

        /// Volume and share path on the Synology to create the LUN on normally /volume1
        #[arg(long, default_value = "/volume1")]
        volume: String,
    },

    /// Delete LUNs and associated Targets from the Synology NAS.
    ///
    /// Exactly one of --name or --prefix must be supplied.
    /// --name to match a single LUN/target name.
    /// --prefix matches all LUNs/targets whose names start with the string,
    /// mostly here for the k8s syno csi which creates a heap of targets.
    Delete {
        #[command(flatten)]
        conn: SynologyConnection,

        /// Exact name of the LUN and associated Target to delete
        #[arg(long, conflicts_with = "prefix", required_unless_present = "prefix")]
        name: Option<String>,

        /// Delete all LUNs and Targets whose names start with this prefix
        #[arg(long, conflicts_with = "name", required_unless_present = "name")]
        prefix: Option<String>,

        /// Show what would be deleted without changes
        #[arg(long)]
        dry_run: bool,
    },

    /// List all LUNs and their associated Targets on the Synology NAS
    List {
        #[command(flatten)]
        conn: SynologyConnection,
    },
}

/// Returns an error if authentication against the Synology WebAPI fails, any
/// of the underlying `SYNO.Core.ISCSI.*` calls report failure, or for
/// `delete` one or more LUNs could not be removed for some reason.
pub async fn run(command: SynologyCommand) -> Result<()> {
    let SynologyCommand::Iscsi { command } = command;
    match command {
        IscsiCommand::Create {
            conn,
            name,
            size,
            allocation,
            volume,
        } => iscsi_create(&conn, &name, size, allocation, &volume).await,
        IscsiCommand::Delete {
            conn,
            name,
            prefix,
            dry_run,
        } => iscsi_delete(&conn, name.as_deref(), prefix.as_deref(), dry_run).await,
        IscsiCommand::List { conn } => iscsi_list(&conn).await,
    }
}

/// Session handle to the Synology WebAPI, scoped only to an `iSCSI` session.
struct Session {
    client: HttpClient,
    entry_url: String,
    auth_url: String,
    sid: String,
}

impl Session {
    async fn login(conn: &SynologyConnection) -> Result<Self> {
        let scheme = if conn.https { "https" } else { "http" };
        let base_url = format!("{scheme}://{}:{}", conn.host, conn.port);
        let entry_url = format!("{base_url}/webapi/entry.cgi");
        let auth_url = format!("{base_url}/webapi/auth.cgi");

        let client = HttpClient::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .wrap_err("failed to build HTTP client")?;

        let login_resp: Value = client
            .post(&auth_url)
            .form(&[
                ("api", "SYNO.API.Auth"),
                ("version", "3"),
                ("method", "login"),
                ("account", conn.user.as_str()),
                ("passwd", conn.password.as_str()),
                ("session", "iSCSI"),
                ("format", "sid"),
            ])
            .send()
            .await
            .wrap_err("login request failed")?
            .json()
            .await
            .wrap_err("failed to parse login response")?;

        if !is_success(&login_resp) {
            bail!("Synology login failed: {}", error_code(&login_resp));
        }

        let Some(sid) = login_resp["data"]["sid"].as_str() else {
            bail!("no sid in login response");
        };

        Ok(Self {
            client,
            entry_url,
            auth_url,
            sid: sid.to_string(),
        })
    }

    /// Best-effort logout, it'll get expired on its own later.
    async fn logout(&self) {
        let _ = self
            .client
            .post(&self.auth_url)
            .form(&[
                ("api", "SYNO.API.Auth"),
                ("version", "1"),
                ("method", "logout"),
                ("session", "iSCSI"),
                ("_sid", self.sid.as_str()),
            ])
            .send()
            .await;
    }

    async fn call(&self, form: &[(&str, &str)]) -> Result<Value> {
        let mut full = form.to_vec();
        full.push(("_sid", self.sid.as_str()));
        self.client
            .post(&self.entry_url)
            .form(&full)
            .send()
            .await
            .wrap_err("entry.cgi request failed")?
            .json()
            .await
            .wrap_err("failed to parse entry.cgi response")
    }
}

fn is_success(v: &Value) -> bool {
    v["success"].as_bool() == Some(true)
}

fn error_code(v: &Value) -> String {
    v["error"]["code"]
        .as_i64()
        .map(|c| c.to_string())
        .unwrap_or_else(|| v.to_string())
}

// The synology api is super inconsistent with `target_id` being a string,
// integer or even inside of arrays. Try to normalize it on our sise.
fn target_id_string(v: &Value) -> Option<String> {
    v["target_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| v["target_id"].as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_str().map(str::to_string))
        .or_else(|| v.as_i64().map(|n| n.to_string()))
}

async fn iscsi_create(
    conn: &SynologyConnection,
    name: &str,
    size: ByteSize,
    allocation: LunAllocation,
    volume: &str,
) -> Result<()> {
    println!("Logging in to Synology at {}:{}", conn.host, conn.port);
    let session = Session::login(conn).await?;
    println!("Login successful");

    let result = create_lun_and_target(&session, name, size, allocation, volume).await;
    session.logout().await;
    let (lun_uuid, target_id, iqn) = result?;

    println!();
    println!("iSCSI resource created on {}:", conn.host);
    println!("  LUN name   : {name}");
    println!("  LUN uuid   : {lun_uuid}");
    println!("  LUN size   : {size}");
    println!("  LUN alloc  : {allocation}");
    println!("  Target name: {name}");
    println!("  Target id  : {target_id}");
    println!("  IQN        : {iqn}");
    println!("  Portal     : {}:3260", conn.host);
    println!();
    println!(
        "To delete: mak synology iscsi delete --host {} --name {name}",
        conn.host
    );

    Ok(())
}

/// Create a LUN, Target, and then map the LUN to the target.
///
/// Mapping failure is only printed as a warning: the LUN and Target still
/// exist and are individually usable even if the final map step didn't take.
///
/// Not sure how to handle that error case as it seems a bit jank.
async fn create_lun_and_target(
    session: &Session,
    name: &str,
    size: ByteSize,
    allocation: LunAllocation,
    volume: &str,
) -> Result<(String, String, String)> {
    let size_str = size.as_u64().to_string();

    println!("Creating LUN '{name}' ({size}, {allocation}) on {volume}");
    let lun_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "create"),
            ("name", name),
            ("type", allocation.as_api_type()),
            ("location", volume),
            ("size", size_str.as_str()),
        ])
        .await?;
    if !is_success(&lun_resp) {
        bail!("LUN creation failed: {}", error_code(&lun_resp));
    }
    let Some(lun_uuid) = lun_resp["data"]["uuid"].as_str() else {
        bail!("no uuid found in LUN create response");
    };
    let lun_uuid = lun_uuid.to_string();
    println!("LUN created: uuid={lun_uuid}");

    // IQN format: iqn.2000-01.com.synology:<name>
    let iqn = format!("iqn.2000-01.com.synology:{name}");
    println!("Creating Target '{name}' iqn={iqn}");
    let target_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.Target"),
            ("version", "1"),
            ("method", "create"),
            ("name", name),
            ("iqn", iqn.as_str()),
            ("auth_type", "0"), // 0 = no CHAP
        ])
        .await?;
    if !is_success(&target_resp) {
        bail!("Target creation failed: {}", error_code(&target_resp));
    }
    let Some(target_id) = target_id_string(&target_resp["data"]) else {
        bail!("no target_id in Target create response");
    };
    println!("Target created: target_id={target_id}");

    println!("Mapping LUN {lun_uuid} to Target {target_id}");
    let map_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "map_target"),
            ("uuid", lun_uuid.as_str()),
            ("target_ids", format!("[\"{target_id}\"]").as_str()),
        ])
        .await?;
    if is_success(&map_resp) {
        println!("LUN mapped to Target successfully");
    } else {
        eprintln!(
            "warning: map_target returned: {map_resp} LUN and Target were still created however"
        );
    }

    Ok((lun_uuid, target_id, iqn))
}

async fn iscsi_list(conn: &SynologyConnection) -> Result<()> {
    let session = Session::login(conn).await?;
    let result = fetch_luns_and_targets(&session).await;
    session.logout().await;
    let (luns, targets) = result?;

    if luns.is_empty() {
        println!("No LUNs found on {}", conn.host);
        return Ok(());
    }

    print_lun_table(&luns, &targets);
    Ok(())
}

async fn fetch_luns_and_targets(session: &Session) -> Result<(Vec<Value>, Vec<Value>)> {
    let lun_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "list"),
        ])
        .await?;
    if !is_success(&lun_resp) {
        bail!("LUN list failed: {lun_resp}");
    }

    let target_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.Target"),
            ("version", "1"),
            ("method", "list"),
        ])
        .await?;
    if !is_success(&target_resp) {
        bail!("Target list failed: {target_resp}");
    }

    let luns = lun_resp["data"]["luns"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let targets = target_resp["data"]["targets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok((luns, targets))
}

/// One printed row of `iscsi list`. A LUN mapped to several Targets is several
/// separate, with the LUN columns left blank after the first so each Target
/// still lands on its own line. The synology is freaking wack.
#[derive(Tabled)]
struct LunRow {
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "UUID")]
    uuid: String,
    #[tabled(rename = "SIZE")]
    size: String,
    #[tabled(rename = "ALLOC")]
    alloc: String,
    #[tabled(rename = "TARGET")]
    target: String,
    #[tabled(rename = "IQN")]
    iqn: String,
}

/// Correlate LUNs with the Targets they are mapped to and print a borderless
/// table.
fn print_lun_table(luns: &[Value], targets: &[Value]) {
    println!("{}", render_lun_table(luns, targets));
}

/// Build the rows and render them.
fn render_lun_table(luns: &[Value], targets: &[Value]) -> String {
    let target_map: HashMap<String, (&str, &str)> = targets
        .iter()
        .filter_map(|t| {
            let id = target_id_string(t)?;
            let name = t["name"].as_str().unwrap_or("?");
            let iqn = t["iqn"].as_str().unwrap_or("?");
            Some((id, (name, iqn)))
        })
        .collect();

    let mut rows = Vec::new();
    for lun in luns {
        let name = lun["name"].as_str().unwrap_or("?").to_string();
        let uuid = lun["uuid"].as_str().unwrap_or("?").to_string();
        let size = ByteSize::b(lun["size"].as_u64().unwrap_or(0)).to_string();
        let alloc = if lun["type"].as_str() == Some("BLUN_THICK") {
            "thick"
        } else {
            "thin"
        }
        .to_string();

        // A LUN can be mapped to multiple targets.
        let mapped: Vec<String> = lun["mapped_targets"]
            .as_array()
            .map(|arr| arr.iter().filter_map(target_id_string).collect())
            .unwrap_or_default();

        if mapped.is_empty() {
            rows.push(LunRow {
                name,
                uuid,
                size,
                alloc,
                target: "(unmapped)".to_string(),
                iqn: String::new(),
            });
            continue;
        }

        for (i, tid) in mapped.iter().enumerate() {
            let (t_name, t_iqn) = target_map.get(tid).copied().unwrap_or(("?", "?"));
            rows.push(if i == 0 {
                LunRow {
                    name: name.clone(),
                    uuid: uuid.clone(),
                    size: size.clone(),
                    alloc: alloc.clone(),
                    target: t_name.to_string(),
                    iqn: t_iqn.to_string(),
                }
            } else {
                // Additional targets for the same LUN continue on their own line.
                LunRow {
                    name: String::new(),
                    uuid: String::new(),
                    size: String::new(),
                    alloc: String::new(),
                    target: t_name.to_string(),
                    iqn: t_iqn.to_string(),
                }
            });
        }
    }

    let mut table = Table::new(rows);
    table.with(Style::blank());
    table.modify(Columns::one(2), Alignment::right()); // SIZE
    table.to_string()
}

async fn iscsi_delete(
    conn: &SynologyConnection,
    name: Option<&str>,
    prefix: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        eprintln!("dry-run: no changes will be made");
    }

    println!("Logging in to Synology at {}:{}", conn.host, conn.port);
    let session = Session::login(conn).await?;
    println!("Login successful");

    let result = delete_matching(&session, name, prefix, dry_run).await;
    session.logout().await;
    let (targets_ok, targets_failed, luns_ok, luns_failed) = result?;

    println!();
    if dry_run {
        println!("dry-run: {targets_ok} target(s) and {luns_ok} LUN(s) would be deleted");
        return Ok(());
    }

    println!("Deletion complete:");
    println!("  Targets : {targets_ok} deleted, {targets_failed} failed");
    println!("  LUNs    : {luns_ok} deleted, {luns_failed} failed");
    if luns_failed > 0 {
        bail!(
            "{luns_failed} LUN(s) failed to delete they may have snapshots, clones, or require manual cleanup, good luck with that!"
        );
    }

    Ok(())
}

/// Does this resource name match the given `--name` or `--prefix` filter?
fn name_matches(candidate: &str, name: Option<&str>, prefix: Option<&str>) -> bool {
    match (name, prefix) {
        (Some(exact), _) => candidate == exact,
        (None, Some(pfx)) => candidate.starts_with(pfx),
        (None, None) => false,
    }
}

/// Delete every Target and LUN whose name matches the filter and return
/// `(targets_ok, targets_failed, luns_ok, luns_failed)`.
///
/// Targets are deleted before LUNs as DSM rejects a LUN delete while it is
/// still mapped to a Target (error 18990505). Still not sure this is right.
async fn delete_matching(
    session: &Session,
    name: Option<&str>,
    prefix: Option<&str>,
    dry_run: bool,
) -> Result<(usize, usize, usize, usize)> {
    let target_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.Target"),
            ("version", "1"),
            ("method", "list"),
        ])
        .await?;
    if !is_success(&target_resp) {
        bail!("Target list failed: {target_resp}");
    }
    let empty = Vec::new();
    let all_targets = target_resp["data"]["targets"].as_array().unwrap_or(&empty);

    let matching_targets: Vec<(String, String)> = all_targets
        .iter()
        .filter_map(|t| {
            let tname = t["name"].as_str()?;
            name_matches(tname, name, prefix)
                .then(|| target_id_string(t))
                .flatten()
                .map(|tid| (tname.to_string(), tid))
        })
        .collect();

    if matching_targets.is_empty() {
        println!("No matching Targets found");
    }

    let target_futures = matching_targets
        .into_iter()
        .map(|(tname, tid)| delete_target(session, tname, tid, dry_run));
    let target_results = futures_util::future::join_all(target_futures).await;
    let targets_ok = target_results.iter().filter(|(_, r)| r.is_ok()).count();
    let targets_failed = target_results.len() - targets_ok;
    for (tname, res) in &target_results {
        if let Err(e) = res {
            eprintln!("warning: Target '{tname}' deletion failed: {e}");
        }
    }

    let lun_resp = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "list"),
        ])
        .await?;
    if !is_success(&lun_resp) {
        bail!("LUN list failed: {lun_resp}");
    }
    let all_luns = lun_resp["data"]["luns"].as_array().unwrap_or(&empty);

    let matching_luns: Vec<(String, String)> = all_luns
        .iter()
        .filter_map(|l| {
            let lname = l["name"].as_str()?;
            if !name_matches(lname, name, prefix) {
                return None;
            }
            Some((lname.to_string(), l["uuid"].as_str()?.to_string()))
        })
        .collect();

    if matching_luns.is_empty() {
        println!("No matching LUNs found");
    }

    let lun_futures = matching_luns
        .into_iter()
        .map(|(lname, uuid)| delete_lun(session, lname, uuid, dry_run));
    let lun_results = futures_util::future::join_all(lun_futures).await;
    let luns_ok = lun_results.iter().filter(|(_, r)| r.is_ok()).count();
    let luns_failed = lun_results.len() - luns_ok;
    for (lname, res) in &lun_results {
        if let Err(e) = res {
            eprintln!("warning: LUN '{lname}' deletion failed: {e}");
        }
    }

    Ok((targets_ok, targets_failed, luns_ok, luns_failed))
}

async fn delete_target(
    session: &Session,
    name: String,
    target_id: String,
    dry_run: bool,
) -> (String, std::result::Result<(), String>) {
    if dry_run {
        eprintln!("dry-run: would delete Target '{name}' target_id={target_id}");
        return (name, Ok(()));
    }

    println!("Deleting Target '{name}' (target_id={target_id})");

    let _ = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.Target"),
            ("version", "1"),
            ("method", "set"),
            ("target_id", target_id.as_str()),
            ("enable", "false"),
        ])
        .await;
    let _ = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.Target"),
            ("version", "1"),
            ("method", "set"),
            ("target_id", target_id.as_str()),
            ("acl_enable", "false"),
        ])
        .await;

    // Delete, Synology requires target_id as a quoted string for this, this api
    // is the freaking worst.
    let quoted = format!("\"{target_id}\"");
    match session
        .call(&[
            ("api", "SYNO.Core.ISCSI.Target"),
            ("version", "1"),
            ("method", "delete"),
            ("target_id", quoted.as_str()),
        ])
        .await
    {
        Ok(resp) if is_success(&resp) => {
            println!("    Target '{name}' deleted");
            (name, Ok(()))
        }
        Ok(resp) => (name, Err(error_code(&resp))),
        Err(e) => (name, Err(format!("request error: {e}"))),
    }
}

async fn delete_lun(
    session: &Session,
    name: String,
    uuid: String,
    dry_run: bool,
) -> (String, std::result::Result<(), String>) {
    if dry_run {
        eprintln!("dry-run: would delete LUN '{name}' (uuid={uuid})");
        return (name, Ok(()));
    }

    println!("Deleting LUN '{name}' (uuid={uuid})");

    let _ = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "set"),
            ("uuid", uuid.as_str()),
            ("is_action_locked", "false"),
        ])
        .await;
    let _ = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "unmap_target"),
            ("uuid", uuid.as_str()),
        ])
        .await;

    if try_delete_lun(session, &uuid).await {
        println!("    LUN '{name}' deleted");
        (name, Ok(()))
    } else {
        (name, Err("all delete attempts failed".to_string()))
    }
}

/// Try up to three delete strategies for a LUN, returning true on first
/// success. DSM firmware versions disagree on whether `uuid` must be quoted or
/// whether `force` is required, so we just try each in turn, I have no idea
/// what the right thing to do is.
async fn try_delete_lun(session: &Session, uuid: &str) -> bool {
    let plain = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "delete"),
            ("uuid", uuid),
        ])
        .await;
    if matches!(&plain, Ok(v) if is_success(v)) {
        return true;
    }

    let forced = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "delete"),
            ("uuid", uuid),
            ("force", "true"),
        ])
        .await;
    if matches!(&forced, Ok(v) if is_success(v)) {
        return true;
    }

    let quoted = format!("\"{uuid}\"");
    let requoted = session
        .call(&[
            ("api", "SYNO.Core.ISCSI.LUN"),
            ("version", "1"),
            ("method", "delete"),
            ("uuid", quoted.as_str()),
        ])
        .await;
    matches!(&requoted, Ok(v) if is_success(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_maps_to_api_type() {
        assert_eq!(LunAllocation::Thin.as_api_type(), "BLUN");
        assert_eq!(LunAllocation::Thick.as_api_type(), "BLUN_THICK");
    }

    #[test]
    fn allocation_displays_lowercase() {
        assert_eq!(LunAllocation::Thin.to_string(), "thin");
        assert_eq!(LunAllocation::Thick.to_string(), "thick");
    }

    #[test]
    fn exact_name_matches_only_that_name() {
        assert!(name_matches("steam", Some("steam"), None));
        assert!(!name_matches("steam2", Some("steam"), None));
    }

    #[test]
    fn prefix_matches_startswith() {
        assert!(name_matches("k8s-csi-pvc-abc", None, Some("k8s-csi-pvc-")));
        assert!(!name_matches("steam", None, Some("k8s-csi-pvc-")));
    }

    #[test]
    fn no_filter_matches_nothing() {
        assert!(!name_matches("anything", None, None));
    }

    #[test]
    fn lun_table_has_no_box_drawing_characters() {
        let luns = vec![serde_json::json!({
            "name": "steam",
            "uuid": "0ac2b072-def3-41e9-b21c-a198ba28af45",
            "size": 11_000_000_000_000u64,
            "type": "BLUN",
            "mapped_targets": [],
        })];

        let out = render_lun_table(&luns, &[]);

        assert!(!out.contains('+'), "no box-drawing corners: {out}");
        assert!(!out.contains('|'), "no box-drawing verticals: {out}");
        // UUIDs legitimately contain hyphens
        assert!(
            !out.lines().any(|l| l.trim().chars().all(|c| c == '-')),
            "no dashed header-separator line: {out}"
        );
        assert!(out.contains("NAME"));
        assert!(out.contains("(unmapped)"));
    }

    #[test]
    fn lun_table_puts_each_mapped_target_on_its_own_row() {
        let luns = vec![serde_json::json!({
            "name": "data",
            "uuid": "686cf5eb-0f49-41d6-af00-e8077eb04632",
            "size": 1_073_700_000u64,
            "type": "BLUN_THICK",
            "mapped_targets": [{"target_id": "1"}, {"target_id": "2"}],
        })];
        let targets = vec![
            serde_json::json!({"target_id": "1", "name": "data-a", "iqn": "iqn.a"}),
            serde_json::json!({"target_id": "2", "name": "data-b", "iqn": "iqn.b"}),
        ];

        let out = render_lun_table(&luns, &targets);

        assert!(out.contains("thick"));
        assert!(out.contains("data-a"));
        assert!(out.contains("iqn.a"));
        assert!(out.contains("data-b"));
        assert!(out.contains("iqn.b"));
        // The LUN's uuid is unique in the output, so it only shows up once.
        assert_eq!(
            out.matches("686cf5eb-0f49-41d6-af00-e8077eb04632").count(),
            1
        );
    }

    #[test]
    fn target_id_string_handles_object_and_bare_forms() {
        assert_eq!(
            target_id_string(&serde_json::json!({"target_id": "5"})),
            Some("5".to_string())
        );
        assert_eq!(
            target_id_string(&serde_json::json!({"target_id": 5})),
            Some("5".to_string())
        );
        assert_eq!(
            target_id_string(&serde_json::json!("5")),
            Some("5".to_string())
        );
        assert_eq!(
            target_id_string(&serde_json::json!(5)),
            Some("5".to_string())
        );
        assert_eq!(target_id_string(&serde_json::json!({})), None);
    }
}
