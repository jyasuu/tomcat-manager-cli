//! tomcat-manager-cli — Apache Tomcat Manager HTTP API CLI
//!
//! Build:   cargo build --release
//! Install: cargo install --path .
//!
//! Shell-script friendly by design:
//!   - Every command supports  --output plain|tsv|json
//!   - Plain / TSV modes print NO colour, NO table borders → safe for awk/grep/jq
//!   - Exit codes: 0 = success, 1 = error (+ message on stderr)
//!   - All human-facing progress text goes to stderr; data goes to stdout
//!
//! Example pipeline — expire sessions idle > 60 min across several apps:
//!
//!   tomcat list -o tsv \
//!     | awk -F'\t' '$2=="running" {print $1}' \
//!     | xargs -I{} tomcat sessions {} -o tsv \
//!     | awk -F'\t' '$1 > 60 {print $2, $3}' \
//!     | while read path idle; do
//!         tomcat expire-sessions "$path" --idle "$idle"
//!       done

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tabled::{Table, Tabled};

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Output format
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Output format for machine-readable commands.
///
/// plain  — space/colon separated, no colour (default for scripts)
/// tsv    — tab-separated values, no header, no colour
/// json   — JSON array of objects
/// table  — pretty ASCII table with colour (default for humans / TTY)
#[derive(Clone, Debug, ValueEnum, Default, PartialEq)]
enum OutputFmt {
    #[default]
    Table,
    Plain,
    Tsv,
    Json,
}

impl OutputFmt {
    /// True when we should suppress all colour and decorations
    fn is_machine(&self) -> bool {
        matches!(self, OutputFmt::Plain | OutputFmt::Tsv | OutputFmt::Json)
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CLI
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Parser, Debug)]
#[command(
    name = "tomcat",
    about = "Apache Tomcat Manager API CLI — shell-script friendly",
    version,
    long_about = "Manage Apache Tomcat via the Manager HTTP API.\n\
                  \n\
                  All commands accept  -o / --output table|plain|tsv|json\n\
                  Use  --output tsv  or  --output json  for awk / jq pipelines.\n\
                  Progress/info messages always go to STDERR; data to STDOUT.\n\
                  Exit codes: 0 = success, 1 = error.\n\
                  \n\
                  Credentials (highest priority first):\n\
                  1. CLI flags  -U / -P / -u\n\
                  2. Env vars   TOMCAT_URL / TOMCAT_USER / TOMCAT_PASSWORD\n\
                  3. Named profile  --profile <name>"
)]
struct Cli {
    /// Tomcat base URL
    #[arg(short, long, env = "TOMCAT_URL")]
    url: Option<String>,

    /// Manager username
    #[arg(short = 'U', long, env = "TOMCAT_USER")]
    username: Option<String>,

    /// Manager password
    #[arg(short = 'P', long, env = "TOMCAT_PASSWORD")]
    password: Option<String>,

    /// Manager context path
    #[arg(long, default_value = "/manager")]
    manager_path: String,

    /// Named connection profile
    #[arg(long, short = 'r')]
    profile: Option<String>,

    /// Print HTTP request/response to stderr
    #[arg(long, short = 'v', global = true)]
    verbose: bool,

    /// Retry attempts on connection failure
    #[arg(long, default_value = "1", global = true)]
    retries: u32,

    /// Seconds between retries
    #[arg(long, default_value = "3", global = true)]
    retry_delay: u64,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List deployed applications
    ///
    /// TSV columns: path  status  sessions  description
    #[command(alias = "ls")]
    List {
        /// Output format
        #[arg(short, long, value_enum, default_value = "table")]
        output: OutputFmt,
    },

    /// Deploy a WAR file (upload or URL)
    Deploy {
        #[arg(short, long)]
        path: String,
        #[arg(short, long, conflicts_with = "war_url")]
        file: Option<PathBuf>,
        #[arg(long)]
        war_url: Option<String>,
        #[arg(long)]
        update: bool,
    },

    /// Undeploy an application
    Undeploy { path: String },

    /// Start a stopped application
    Start { path: String },

    /// Stop a running application
    Stop { path: String },

    /// Reload an application (stop + start)
    Reload { path: String },

    /// Poll until an app reaches 'running' (useful after deploy in CI)
    Wait {
        path: String,
        #[arg(short, long, default_value = "60")]
        timeout: u64,
        #[arg(long, default_value = "2")]
        interval: u64,
    },

    /// Print server information
    ///
    /// TSV columns: key  value
    Info {
        #[arg(short, long, value_enum, default_value = "table")]
        output: OutputFmt,
    },

    /// Print server status (memory / connectors)
    Status,

    /// Trigger a JVM garbage collection
    #[command(name = "gc")]
    GarbageCollect,

    /// Print session idle-time histogram for one app
    ///
    /// TSV columns: idle_minutes  session_count  app_path
    ///
    /// This is the primary command for shell pipelines — each output line
    /// represents one idle-time bucket that can be grepped/awk-ed and then
    /// piped into expire-sessions.
    Sessions {
        /// Context path
        path: String,

        /// Output format (tsv recommended for pipelines)
        #[arg(short, long, value_enum, default_value = "table")]
        output: OutputFmt,
    },

    /// Expire sessions idle >= N minutes  (-1 = all)
    ///
    /// Can read (path, idle) pairs from stdin when --stdin is set:
    ///   echo "/myapp 60" | tomcat expire-sessions --stdin
    ExpireSessions {
        /// Context path (ignored when --stdin is used)
        #[arg(required_unless_present = "stdin")]
        path: Option<String>,
        #[arg(short, long, default_value = "-1")]
        idle: i32,
        /// Read newline-delimited "path idle_minutes" pairs from stdin
        #[arg(long)]
        stdin: bool,
    },

    /// Find apps leaking memory across reloads
    ///
    /// TSV: app_path
    FindLeakers {
        #[arg(short, long, value_enum, default_value = "table")]
        output: OutputFmt,
    },

    /// List SSL connector ciphers
    SslConnectors,

    /// Manage saved connection profiles
    #[command(subcommand)]
    Profile(ProfileCommands),
}

#[derive(Subcommand, Debug)]
enum ProfileCommands {
    /// Save a connection profile
    Set {
        name: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: String,
    },
    /// List all saved profiles  (TSV: name  url  username)
    List,
    /// Delete a saved profile
    Delete { name: String },
    /// Print the profiles file path
    Path,
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Profile storage
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug, Clone, Default)]
struct ProfileEntry {
    url: String,
    username: String,
    password: String,
}

fn profiles_path() -> PathBuf {
    let base = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(base)
        .join(".config")
        .join("tomcat-cli")
        .join("profiles.toml")
}

fn load_profiles() -> HashMap<String, ProfileEntry> {
    let Ok(content) = std::fs::read_to_string(profiles_path()) else {
        return HashMap::new();
    };
    let mut map: HashMap<String, ProfileEntry> = HashMap::new();
    let mut current: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("[profile.") && line.ends_with(']') {
            let name = line[9..line.len() - 1].to_string();
            map.entry(name.clone()).or_default();
            current = Some(name);
        } else if let Some(ref name) = current
            && let Some((k, v)) = line.split_once('=')
        {
            let v = v.trim().trim_matches('"');
            let e = map.entry(name.clone()).or_default();
            match k.trim() {
                "url" => e.url = v.into(),
                "username" => e.username = v.into(),
                "password" => e.password = v.into(),
                _ => {}
            }
        }
    }
    map
}

fn save_profiles(map: &HashMap<String, ProfileEntry>) -> Result<()> {
    let path = profiles_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut out = String::new();
    for (name, e) in map {
        out.push_str(&format!(
            "[profile.{}]\nurl = \"{}\"\nusername = \"{}\"\npassword = \"{}\"\n\n",
            name, e.url, e.username, e.password
        ));
    }
    std::fs::write(path, out).context("failed to write profiles")?;
    Ok(())
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Connection resolution
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

struct Connection {
    url: String,
    username: String,
    password: String,
}

fn resolve_connection(cli: &Cli) -> Result<Connection> {
    let def = if let Some(ref name) = cli.profile {
        load_profiles()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("profile '{}' not found", name))?
    } else {
        ProfileEntry::default()
    };

    let fallback = |opt: &Option<String>, d: &str| -> String {
        opt.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| {
            if d.is_empty() {
                String::new()
            } else {
                d.to_string()
            }
        })
    };

    Ok(Connection {
        url: fallback(
            &cli.url,
            if def.url.is_empty() {
                "http://localhost:8080"
            } else {
                &def.url
            },
        ),
        username: fallback(
            &cli.username,
            if def.username.is_empty() {
                "admin"
            } else {
                &def.username
            },
        ),
        password: cli.password.clone().unwrap_or(def.password),
    })
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// HTTP client
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

struct TomcatClient {
    client: Client,
    base_url: String,
    status_url: String,
    username: String,
    password: String,
    verbose: bool,
    retries: u32,
    retry_delay: Duration,
}

impl TomcatClient {
    fn new(
        conn: &Connection,
        manager_path: &str,
        verbose: bool,
        retries: u32,
        retry_delay: u64,
    ) -> Self {
        let root = format!(
            "{}{}",
            conn.url.trim_end_matches('/'),
            manager_path.trim_end_matches('/')
        );
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap(),
            base_url: format!("{}/text", root),
            status_url: format!("{}/status", root),
            username: conn.username.clone(),
            password: conn.password.clone(),
            verbose,
            retries,
            retry_delay: Duration::from_secs(retry_delay),
        }
    }

    fn get(&self, endpoint: &str, params: &[(&str, &str)]) -> Result<String> {
        let url = format!("{}{}", self.base_url, endpoint);
        self.with_retry(|| {
            if self.verbose {
                eprintln!("[http] GET {}  {:?}", url, params);
            }
            let resp = self
                .client
                .get(&url)
                .basic_auth(&self.username, Some(&self.password))
                .query(params)
                .send()
                .with_context(|| format!("connection failed: {}", url))?;
            let st = resp.status();
            let body = resp.text()?;
            if self.verbose {
                eprintln!("[http] {} body: {}", st, body.trim());
            }
            if !st.is_success() {
                return Err(anyhow!("HTTP {}: {}", st, body.trim()));
            }
            check_tomcat_response(&body)?;
            Ok(body)
        })
    }

    fn get_raw(&self, url: &str) -> Result<String> {
        self.with_retry(|| {
            let resp = self
                .client
                .get(url)
                .basic_auth(&self.username, Some(&self.password))
                .send()
                .with_context(|| format!("connection failed: {}", url))?;
            Ok(resp.text()?)
        })
    }

    fn put_war(&self, endpoint: &str, params: &[(&str, &str)], file: &PathBuf) -> Result<String> {
        let url = format!("{}{}", self.base_url, endpoint);
        let bytes =
            std::fs::read(file).with_context(|| format!("cannot read {}", file.display()))?;
        self.with_retry(|| {
            if self.verbose {
                eprintln!("[http] PUT {}  bytes={}", url, bytes.len());
            }
            let resp = self
                .client
                .put(&url)
                .basic_auth(&self.username, Some(&self.password))
                .query(params)
                .header("Content-Type", "application/octet-stream")
                .body(bytes.clone())
                .send()
                .with_context(|| format!("connection failed: {}", url))?;
            let st = resp.status();
            let body = resp.text()?;
            if !st.is_success() {
                return Err(anyhow!("HTTP {}: {}", st, body.trim()));
            }
            check_tomcat_response(&body)?;
            Ok(body)
        })
    }

    fn with_retry<F: Fn() -> Result<String>>(&self, f: F) -> Result<String> {
        let mut last = anyhow!("no attempts");
        for attempt in 0..self.retries {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last = e;
                    if attempt + 1 < self.retries {
                        eprintln!(
                            "↺ attempt {}/{} failed: {} — retrying in {}s…",
                            attempt + 1,
                            self.retries,
                            last,
                            self.retry_delay.as_secs()
                        );
                        std::thread::sleep(self.retry_delay);
                    }
                }
            }
        }
        Err(last)
    }
}

fn check_tomcat_response(body: &str) -> Result<()> {
    let first = body.lines().next().unwrap_or("").trim();
    if first.starts_with("FAIL") {
        return Err(anyhow!(
            "{}",
            first.strip_prefix("FAIL - ").unwrap_or(first)
        ));
    }
    Ok(())
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Data types & parsers
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug, Clone)]
struct AppEntry {
    path: String,
    status: String,
    sessions: String,
    description: String,
}

fn parse_app_list(body: &str) -> Vec<AppEntry> {
    body.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let p: Vec<&str> = line.splitn(4, ':').collect();
            AppEntry {
                path: p.first().copied().unwrap_or("").into(),
                status: p.get(1).copied().unwrap_or("").into(),
                sessions: p.get(2).copied().unwrap_or("0").into(),
                description: p.get(3).copied().unwrap_or("").into(),
            }
        })
        .collect()
}

/// A single row from the /sessions histogram response.
/// Tomcat returns lines like:
///   Session idle times histogram: 10 - 20 minutes: 3 sessions
///   Default maximum session inactive interval 30 minutes
#[derive(Debug, Clone)]
struct SessionBucket {
    /// Lower bound of idle time in minutes (or exact value)
    idle_min: u64,
    /// Session count in this bucket
    count: u64,
    /// The app context path this bucket belongs to
    path: String,
}

/// Parse the /sessions text response for one app into structured buckets.
///
/// Raw response example:
///   OK - Session information for application at context path /myapp
///   Default maximum session inactive interval 30 minutes
///   <1 minutes: 5 sessions
///   1 - 5 minutes: 12 sessions
///   5 - 10 minutes: 3 sessions
///   10 - 20 minutes: 1 sessions
fn parse_sessions(body: &str, path: &str) -> Vec<SessionBucket> {
    let mut buckets = Vec::new();
    for line in body.lines().skip(1) {
        let line = line.trim();
        // Match lines ending in "N sessions" or "N session"
        let Some(session_part) = line.rfind(" session") else {
            continue;
        };
        let before = &line[..session_part];
        let Some(colon) = before.rfind(": ") else {
            continue;
        };
        let count_str = before[colon + 2..].trim();
        let Ok(count) = count_str.parse::<u64>() else {
            continue;
        };
        if count == 0 {
            continue;
        }

        let range_str = before[..colon].trim();
        // Extract the lower bound from patterns like:
        //   "<1"  "1 - 5"  "5 - 10"  "30+"
        let idle_min = if let Some(stripped) = range_str.strip_prefix('<') {
            0u64.saturating_add(
                stripped
                    .trim()
                    .parse::<u64>()
                    .unwrap_or(1)
                    .saturating_sub(1),
            )
        } else if let Some(dash_pos) = range_str.find(" - ") {
            range_str[..dash_pos].trim().parse().unwrap_or(0)
        } else {
            range_str.trim_end_matches('+').trim().parse().unwrap_or(0)
        };

        buckets.push(SessionBucket {
            idle_min,
            count,
            path: path.to_string(),
        });
    }
    buckets
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Output helpers
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Print a simple key=value pair in each format.
/// Used by Info, single-value queries.
fn print_kv(pairs: &[(&str, &str)], fmt: &OutputFmt) {
    match fmt {
        OutputFmt::Table => {
            for (k, v) in pairs {
                println!("  {:<30} {}", format!("{}:", k).bold(), v);
            }
        }
        OutputFmt::Plain => {
            for (k, v) in pairs {
                println!("{}={}", k, v);
            }
        }
        OutputFmt::Tsv => {
            for (k, v) in pairs {
                println!("{}\t{}", k, v);
            }
        }
        OutputFmt::Json => {
            let entries: Vec<String> = pairs
                .iter()
                .map(|(k, v)| {
                    format!(
                        "  {{ \"key\": {}, \"value\": {} }}",
                        json_str(k),
                        json_str(v)
                    )
                })
                .collect();
            println!("[\n{}\n]", entries.join(",\n"));
        }
    }
}

/// Print app list in each format.
fn print_app_list(apps: &[AppEntry], fmt: &OutputFmt) {
    match fmt {
        OutputFmt::Table => {
            #[derive(Tabled)]
            struct Row {
                #[tabled(rename = "Path")]
                path: String,
                #[tabled(rename = "Status")]
                status: String,
                #[tabled(rename = "Sessions")]
                sessions: String,
                #[tabled(rename = "Description")]
                description: String,
            }
            let rows: Vec<Row> = apps
                .iter()
                .map(|a| Row {
                    path: a.path.clone(),
                    status: colorize_status(&a.status),
                    sessions: a.sessions.clone(),
                    description: a.description.clone(),
                })
                .collect();
            println!("{}", Table::new(rows));
        }
        OutputFmt::Plain => {
            for a in apps {
                println!("{} {} {} {}", a.path, a.status, a.sessions, a.description);
            }
        }
        OutputFmt::Tsv => {
            for a in apps {
                println!(
                    "{}\t{}\t{}\t{}",
                    a.path, a.status, a.sessions, a.description
                );
            }
        }
        OutputFmt::Json => {
            let entries: Vec<String> = apps.iter().map(|a| {
                format!("  {{ \"path\": {}, \"status\": {}, \"sessions\": {}, \"description\": {} }}",
                    json_str(&a.path), json_str(&a.status), json_str(&a.sessions), json_str(&a.description))
            }).collect();
            println!("[\n{}\n]", entries.join(",\n"));
        }
    }
}

/// Print session buckets in each format.
///
/// TSV columns: idle_minutes  session_count  app_path
/// This is what shell pipelines filter and pass to expire-sessions.
fn print_session_buckets(buckets: &[SessionBucket], fmt: &OutputFmt) {
    match fmt {
        OutputFmt::Table => {
            #[derive(Tabled)]
            struct Row {
                #[tabled(rename = "Idle (min)")]
                idle: u64,
                #[tabled(rename = "Sessions")]
                count: u64,
            }
            let rows: Vec<Row> = buckets
                .iter()
                .map(|b| Row {
                    idle: b.idle_min,
                    count: b.count,
                })
                .collect();
            println!("{}", Table::new(rows));
        }
        OutputFmt::Plain => {
            for b in buckets {
                println!("idle={} count={} path={}", b.idle_min, b.count, b.path);
            }
        }
        OutputFmt::Tsv => {
            // idle_minutes TAB session_count TAB app_path
            for b in buckets {
                println!("{}\t{}\t{}", b.idle_min, b.count, b.path);
            }
        }
        OutputFmt::Json => {
            let entries: Vec<String> = buckets
                .iter()
                .map(|b| {
                    format!(
                        "  {{ \"idle_minutes\": {}, \"count\": {}, \"path\": {} }}",
                        b.idle_min,
                        b.count,
                        json_str(&b.path)
                    )
                })
                .collect();
            println!("[\n{}\n]", entries.join(",\n"));
        }
    }
}

fn colorize_status(s: &str) -> String {
    match s {
        "running" => s.green().to_string(),
        "stopped" => s.red().to_string(),
        _ => s.yellow().to_string(),
    }
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn print_ok(body: &str) {
    let msg = body
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("OK - ")
        .unwrap_or("OK");
    println!("{} {}", "✓".green().bold(), msg);
}

/// Progress/info text — always to stderr so it doesn't pollute pipelines
macro_rules! progress {
    ($($t:tt)*) => { eprintln!($($t)*) }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Command handlers
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn cmd_list(client: &TomcatClient, fmt: &OutputFmt) -> Result<()> {
    let body = client.get("/list", &[])?;
    let apps = parse_app_list(&body);
    if apps.is_empty() && !fmt.is_machine() {
        progress!("{}", "No applications deployed.".yellow());
    } else {
        print_app_list(&apps, fmt);
    }
    Ok(())
}

fn cmd_deploy(
    client: &TomcatClient,
    path: &str,
    file: Option<PathBuf>,
    war_url: Option<String>,
    update: bool,
) -> Result<()> {
    let upd = if update { "true" } else { "false" };
    match (file, war_url) {
        (Some(ref f), _) => {
            let kb = std::fs::metadata(f)
                .map(|m| format!("{:.1} KB", m.len() as f64 / 1024.0))
                .unwrap_or_default();
            progress!("Deploying {} from file {} ({})…", path, f.display(), kb);
            print_ok(&client.put_war("/deploy", &[("path", path), ("update", upd)], f)?);
        }
        (_, Some(ref u)) => {
            progress!("Deploying {} from URL {}…", path, u);
            print_ok(&client.get("/deploy", &[("path", path), ("war", u), ("update", upd)])?);
        }
        _ => return Err(anyhow!("Provide --file or --war-url")),
    }
    Ok(())
}

fn cmd_undeploy(client: &TomcatClient, path: &str) -> Result<()> {
    progress!("Undeploying {}…", path);
    print_ok(&client.get("/undeploy", &[("path", path)])?);
    Ok(())
}

fn cmd_start(client: &TomcatClient, path: &str) -> Result<()> {
    progress!("Starting {}…", path);
    print_ok(&client.get("/start", &[("path", path)])?);
    Ok(())
}

fn cmd_stop(client: &TomcatClient, path: &str) -> Result<()> {
    progress!("Stopping {}…", path);
    print_ok(&client.get("/stop", &[("path", path)])?);
    Ok(())
}

fn cmd_reload(client: &TomcatClient, path: &str) -> Result<()> {
    progress!("Reloading {}…", path);
    print_ok(&client.get("/reload", &[("path", path)])?);
    Ok(())
}

fn cmd_wait(client: &TomcatClient, path: &str, timeout: u64, interval: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let poll = Duration::from_secs(interval);
    progress!("Waiting for {} to be running (timeout {}s)…", path, timeout);
    loop {
        let body = client.get("/list", &[])?;
        let status = parse_app_list(&body)
            .into_iter()
            .find(|a| a.path == path)
            .map(|a| a.status);
        match status.as_deref() {
            Some("running") => {
                // Print to stdout so scripts can detect success
                println!("running");
                return Ok(());
            }
            Some(s) => progress!("  status: {}…", s),
            None => progress!("  not found yet…"),
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timeout: {} did not reach 'running' in {}s",
                path,
                timeout
            ));
        }
        std::thread::sleep(poll);
    }
}

fn cmd_info(client: &TomcatClient, fmt: &OutputFmt) -> Result<()> {
    let body = client.get("/serverinfo", &[])?;
    let pairs: Vec<(String, String)> = body
        .lines()
        .skip(1)
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    print_kv(&borrowed, fmt);
    Ok(())
}

fn cmd_status(client: &TomcatClient) -> Result<()> {
    let body = client.get_raw(&client.status_url.clone())?;
    println!("{}", strip_html(&body).trim());
    Ok(())
}

fn cmd_gc(client: &TomcatClient) -> Result<()> {
    progress!("Requesting garbage collection…");
    print_ok(&client.get("/gc", &[])?);
    Ok(())
}

fn cmd_sessions(client: &TomcatClient, path: &str, fmt: &OutputFmt) -> Result<()> {
    let body = client.get("/sessions", &[("path", path)])?;
    let buckets = parse_sessions(&body, path);
    if buckets.is_empty() {
        if !fmt.is_machine() {
            progress!("No active sessions for {}", path);
        }
        // In machine mode, emit nothing — lets the pipeline terminate gracefully
    } else {
        print_session_buckets(&buckets, fmt);
    }
    Ok(())
}

fn cmd_expire_sessions(client: &TomcatClient, path: &str, idle: i32) -> Result<()> {
    let idle_str = idle.to_string();
    let body = client.get("/expire", &[("path", path), ("idle", &idle_str)])?;
    // Print the raw OK line to stdout so scripts can check it
    println!("{}", body.lines().next().unwrap_or("").trim());
    Ok(())
}

/// Expire sessions by reading "path idle_minutes" pairs from stdin.
/// Each line must be:  /myapp 60
/// Blank lines and lines starting with # are ignored.
fn cmd_expire_from_stdin(client: &TomcatClient) -> Result<()> {
    use std::io::{self, BufRead};
    let stdin = io::stdin();
    let mut count = 0u32;
    for line in stdin.lock().lines() {
        let line = line.context("stdin read error")?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(2, |c: char| c.is_whitespace()).collect();
        match parts.as_slice() {
            [path, idle_str] => {
                let idle: i32 = idle_str.trim().parse().with_context(|| {
                    format!("invalid idle value '{}' on line: {}", idle_str, line)
                })?;
                let idle_s = idle.to_string();
                let body = client.get("/expire", &[("path", path), ("idle", &idle_s)])?;
                println!("{}", body.lines().next().unwrap_or("").trim());
                count += 1;
            }
            _ => eprintln!("warn: skipping malformed line: {}", line),
        }
    }
    progress!("Processed {} expire-session requests.", count);
    Ok(())
}

fn cmd_find_leakers(client: &TomcatClient, fmt: &OutputFmt) -> Result<()> {
    let body = client.get("/findleaks", &[("statusLine", "true")])?;
    let leakers: Vec<&str> = body
        .lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .collect();
    match fmt {
        OutputFmt::Table => {
            if leakers.is_empty() {
                progress!("{} No memory leakers found.", "✓".green().bold());
            } else {
                eprintln!("{}", "Memory leaking applications:".red().bold());
                for l in &leakers {
                    println!("  ● {}", l.yellow());
                }
            }
        }
        OutputFmt::Plain | OutputFmt::Tsv => {
            for l in &leakers {
                println!("{}", l);
            }
        }
        OutputFmt::Json => {
            let entries: Vec<String> = leakers
                .iter()
                .map(|l| format!("  {}", json_str(l)))
                .collect();
            println!("[\n{}\n]", entries.join(",\n"));
        }
    }
    Ok(())
}

fn cmd_ssl_connectors(client: &TomcatClient) -> Result<()> {
    println!("{}", client.get("/sslConnectorCiphers", &[])?.trim());
    Ok(())
}

fn cmd_profile(sub: ProfileCommands) -> Result<()> {
    match sub {
        ProfileCommands::Set {
            name,
            url,
            username,
            password,
        } => {
            let mut p = load_profiles();
            p.insert(
                name.clone(),
                ProfileEntry {
                    url,
                    username,
                    password,
                },
            );
            save_profiles(&p)?;
            progress!("{} Profile '{}' saved.", "✓".green().bold(), name);
        }
        ProfileCommands::List => {
            let p = load_profiles();
            if p.is_empty() {
                progress!("{}", "No profiles saved.".yellow());
            } else {
                for (name, e) in &p {
                    println!("{}\t{}\t{}", name, e.url, e.username);
                }
            }
        }
        ProfileCommands::Delete { name } => {
            let mut p = load_profiles();
            if p.remove(&name).is_none() {
                return Err(anyhow!("profile '{}' not found", name));
            }
            save_profiles(&p)?;
            progress!("{} Profile '{}' deleted.", "✓".green().bold(), name);
        }
        ProfileCommands::Path => println!("{}", profiles_path().display()),
    }
    Ok(())
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Utility
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let mut result = String::new();
    let mut prev_blank = false;
    for line in out.lines() {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        result.push_str(line);
        result.push('\n');
        prev_blank = blank;
    }
    result
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// main
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn main() {
    let cli = Cli::parse();

    if let Commands::Profile(sub) = cli.command {
        if let Err(e) = cmd_profile(sub) {
            die(e);
        }
        return;
    }

    let conn = match resolve_connection(&cli) {
        Ok(c) => c,
        Err(e) => die(e),
    };
    let client = TomcatClient::new(
        &conn,
        &cli.manager_path,
        cli.verbose,
        cli.retries,
        cli.retry_delay,
    );

    let result = match cli.command {
        Commands::List { output } => cmd_list(&client, &output),
        Commands::Deploy {
            path,
            file,
            war_url,
            update,
        } => cmd_deploy(&client, &path, file, war_url, update),
        Commands::Undeploy { path } => cmd_undeploy(&client, &path),
        Commands::Start { path } => cmd_start(&client, &path),
        Commands::Stop { path } => cmd_stop(&client, &path),
        Commands::Reload { path } => cmd_reload(&client, &path),
        Commands::Wait {
            path,
            timeout,
            interval,
        } => cmd_wait(&client, &path, timeout, interval),
        Commands::Info { output } => cmd_info(&client, &output),
        Commands::Status => cmd_status(&client),
        Commands::GarbageCollect => cmd_gc(&client),
        Commands::Sessions { path, output } => cmd_sessions(&client, &path, &output),
        Commands::ExpireSessions { path, idle, stdin } => {
            if stdin {
                cmd_expire_from_stdin(&client)
            } else {
                cmd_expire_sessions(&client, &path.unwrap(), idle)
            }
        }
        Commands::FindLeakers { output } => cmd_find_leakers(&client, &output),
        Commands::SslConnectors => cmd_ssl_connectors(&client),
        Commands::Profile(_) => unreachable!(),
    };

    if let Err(e) = result {
        die(e);
    }
}

fn die(e: anyhow::Error) -> ! {
    eprintln!("{} {}", "✗".red().bold(), e);
    std::process::exit(1);
}
