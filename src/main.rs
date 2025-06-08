#[macro_use] extern crate clap;

#[cfg(test)] extern crate parameterized_test;

use clap::{AppSettings, ArgGroup, Parser};
use std::borrow::Cow;
use std::convert::TryFrom;
use std::ffi::{OsStr, OsString};
use std::time::{Duration, Instant};
use subprocess::{Exec, Redirection, ExitStatus, CaptureData, PopenConfig};
use ureq::{Agent, AgentBuilder, Error, Response};
use uuid::Uuid;

static MAX_BYTES_TO_POST: usize = 10000; // not 10KB, https://healthchecks.io/docs/attaching_logs/
static MAX_STRING_TO_LOG: usize = 1000;

/// Truncates a string for display
fn truncate_str(s: String, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", s.chars().take(max_len-3).collect::<String>())
    } else { s }
}

/// Constructs a User Agent string including the hostname and binary name.
fn make_user_agent(custom: Option<&str>) -> String {
    let base = match hostname::get().ok() {
        Some(host) => format!("{} - {}", crate_name!(), host.to_string_lossy()),
        None => crate_name!().to_string(),
    };

    match custom {
        Some(agent) => format!("{} ({})", agent, base),
        None => base,
    }
}

/// Executes a subprocess, distilling all situations (failures, etc.) to a string of output and an
/// exit code. This is obviously lossy, but is sufficient for our purposes. Setting verbose=true
/// will log lost details to stderr.
fn execute(command: &[impl AsRef<OsStr>], capture_output: bool, verbose: bool) -> (String, u8, Duration) {
    let command = Exec::cmd(&command[0]).args(&command[1..])
        .stdout(Redirection::Pipe)
        .stderr(Redirection::Merge);
    if verbose { eprintln!("About to run: {:?}", command); }

    let start = Instant::now();
    // TODO consider discarding stdout instead of capturing it if !capture_output;
    // subprocess::Communicator::limit_size() can avoid unbounded memory allocation
    let capture = command.capture();
    let elapsed = start.elapsed();

    if verbose {
        match &capture {
            Ok(cap) =>
                eprintln!("stdout+stderr:[{}] exit:{:?} runtime:{:?}",
                          truncate_str(cap.stdout_str(), MAX_STRING_TO_LOG),
                          cap.exit_status,
                          elapsed),
            Err(e) => eprintln!("Failed! {:?} runtime:{:?}", e, elapsed),
        };
    }

    let capture = match capture {
        Ok(cap) => cap,
        Err(e) => CaptureData {
            stdout: format!("{}: Command failed: {}", crate_name!(), e).bytes().collect(),
            stderr: Vec::new(),
            exit_status: ExitStatus::Undetermined,
        },
    };
    assert!(capture.stderr.is_empty(), "No data should have been written to stderr");

    let code = match capture.exit_status {
        ExitStatus::Exited(code) => u8::try_from(code).unwrap_or(127),
        ExitStatus::Signaled(signal) => signal + 128,
        _ => 127,
    };
    (if capture_output { capture.stdout_str() } else { String::new() }, code, elapsed)
}

struct HCAgent {
    agent: Agent,
    verbose: bool,
    url_prefix: String,
}

impl HCAgent {
    fn create(cli: &Cli) -> Self {
        // TODO support retries
        // TODO could potentially shrink the binary size further by manually constructing requests with
        // https://doc.rust-lang.org/std/net/struct.TcpStream.html and https://docs.rs/native-tls/
        let agent = AgentBuilder::new()
            .timeout(Duration::from_secs(10)) // https://healthchecks.io/docs/reliability_tips/
            .user_agent(&make_user_agent(cli.user_agent.as_deref()))
            .build();

        HCAgent { agent, verbose: cli.verbose, url_prefix: cli.url_prefix() }
    }

    /// Pings the Healthchecks server to notify that the task denoted by the URL prefix is starting
    /// A run_id UUID is used to associate this event with its completion notification
    fn notify_start(&self, run_id: Uuid) -> Result<Response, Error> {
        let url = format!("{}/start?rid={}", self.url_prefix, run_id);
        let req = self.agent.get(&url);
        if self.verbose { eprintln!("Sending request: {:?}", req); }
        req.call()
    }

    /// Pings the Healthchecks server to notify that the task denoted by the URL prefix is done.
    /// A run_id UUID is used to associated this event with its start notification, if one was sent
    /// If code is non-zero, the task will be considered failed. If code is None the task will be logged
    /// but not update the check.
    fn notify_complete(&self, run_id: Option<Uuid>, code: Option<u8>, output: &str) -> Result<Response, Error> {
        let mut url = format!("{}/{}", self.url_prefix, code.map(|x| x.to_string()).unwrap_or_else(|| "log".to_string()));
        if let Some(run_id) = run_id {
            url = format!("{}?rid={}", url, run_id);
        }
        let req = self.agent.post(&url);
        if self.verbose { eprintln!("Sending request: {:?}", req); }
        if output.is_empty() {
            req.call()
        } else {
            req.send_string(output)
        }
    }
}

#[derive(Parser)]
#[clap(about, version)]
#[clap(setting = AppSettings::DeriveDisplayOrder)]
#[clap(setting = AppSettings::ArgRequiredElseHelp)]
#[clap(group(ArgGroup::new("label").required(true)))]
struct Cli {
    /// Check's UUID to ping
    #[clap(long, short='k', value_name="UUID", group="label")]
    uuid: Option<String>,

    /// Check's slug name to ping, requires also specifying --ping-key
    #[clap(long, short='s', value_name="SLUG", group="label", requires="ping-key")]
    slug: Option<String>,

    /// Check's project ping key, required when using --slug
    #[clap(long, env="HEALTHCHECKS_PING_KEY", value_name="PING_KEY")]
    ping_key: Option<String>,

    /// Ping when the program starts as well as completes
    #[clap(long, short='t')]
    time: bool,

    /// POST the first 10k bytes instead of the last
    #[clap(long)]
    head: bool,

    /// Don't POST any output from the command
    #[clap(long, conflicts_with_all=&["detailed", "env"])]
    ping_only: bool,

    /// Log the invocation without signalling success or failure; does not update the check's status
    #[clap(long, conflicts_with="time")]
    log: bool,

    /// Include execution details in the information POST-ed (by default just sends stdout/err
    #[clap(long)]
    detailed: bool,

    /// Also POSTs the process environment; requires --detailed
    #[clap(long, requires="detailed")]
    env: bool,

    /// Write debugging details to stderr
    #[clap(long)]
    verbose: bool,

    /// Customize the user-agent string sent to the Healthchecks.io server
    #[clap(long, value_name="USER_AGENT")]
    user_agent: Option<String>,

    /// Base URL of the Healthchecks.io server to ping
    #[clap(long, env="HEALTHCHECKS_BASE_URL", default_value="https://hc-ping.com")]
    base_url: String,

    /// The command to run
    #[clap(required=true, last=true)]
    command: Vec<OsString>,
}

impl Cli {
    fn url_prefix(&self) -> String {
        match &self.uuid {
            Some(uuid) => format!("{}/{}", self.base_url, uuid),
            None => {
                // These expect()s should never be hit in practice because clap enforces either
                // --uuid or --ping_key+--slug.
                let slug = self.slug.as_ref().expect("BUG: Must provide --uuid or --slug");
                let ping_key = self.ping_key.as_ref().expect("BUG: Must provide --ping_key with --slug");
                format!("{}/{}/{}", self.base_url, ping_key, slug)
            }
        }
    }
}

fn run(cli: Cli, agent: HCAgent) -> Result<Response, Error> {
    let mut maybe_run_id = None;  // Don't bother reporting a run ID unless we're sending a start ping
    if cli.time {
        let run_id = Uuid::new_v4();
        maybe_run_id = Some(run_id);
        if let Err(e) = agent.notify_start(run_id) {
            eprintln!("Failed to send start request: {:?}", e);
        }
    }
    let (mut output, code, elapsed) = execute(&cli.command, !cli.ping_only, cli.verbose);

    if cli.detailed {
        // We could properly escape command, e.g. with https://crates.io/crates/shell-quote
        output = format!("$ {} 2>&1\n{}\n\nExit Code: {}\nDuration: {:?}",
                         cli.command.join(OsStr::new(" ")).to_string_lossy(), output, code, elapsed);
        if cli.env {
            let env_str = PopenConfig::current_env().iter()
                .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
                .collect::<Vec<_>>().join("\n");
            output = format!("{}\n{}", env_str, output);
        }
    }

    // If we have too much output safely convert the last 10k bytes into UTF-8
    let output =
        if !cli.head && output.len() > MAX_BYTES_TO_POST {
            String::from_utf8_lossy(&output.as_bytes()[output.len() - MAX_BYTES_TO_POST..])
        } else { Cow::Owned(output) };

    // Trim replacement chars added by from_utf8_lossy since they are multi-byte and can actually
    // increase the length of the string.
    let code = if cli.log { None } else { Some(code) };
    agent.notify_complete(maybe_run_id, code, output.trim_start_matches(|c| c=='�'))
}

fn main() {
    let cli = Cli::parse();
    let agent = HCAgent::create(&cli);

    run(cli, agent).expect("Failed to reach Healthchecks.io");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_cli() {
        use clap::IntoApp;
        Cli::into_app().debug_assert()
    }

    //
    // NOTE: Mockito's state sometimes leaks across tests, so each test should use a separate
    // fake UUID to avoid flaky matches. See https://github.com/lipanski/mockito/issues/111
    //

    parameterized_test::create!{ truncate, (orig, expected), {
        assert_eq!(truncate_str(orig.into(), 10), expected); }
    }
    truncate! {
        short:  ("short", "short"),
        barely: ("barely fit", "barely fit"),
        long:   ("much too long", "much to..."),
    }

    #[test]
    fn agent() {
        // This is mostly a change-detector, but it's helpful to validate the expected format
        match hostname::get().ok() {
            Some(host) => {
                assert_eq!(make_user_agent(None),
                           format!("{} - {}", crate_name!(), host.to_string_lossy()));
                assert_eq!(make_user_agent(Some("foo")),
                           format!("foo ({} - {})", crate_name!(), host.to_string_lossy()));
            },
            None => {
                assert_eq!(make_user_agent(None), crate_name!());
                assert_eq!(make_user_agent(Some("foo")), format!("foo ({})", crate_name!()));
            },
        }
    }

    #[test]
    fn ping() {
        let suc_m = mockito::mock("POST", "/ping/0").match_body("foo bar").with_status(200).create();
        let fail_m = mockito::mock("POST", "/ping/10").match_body("bar baz").with_status(200).create();
        let log_m = mockito::mock("POST", "/ping/log").match_body("bang boom").with_status(200).create();
        let runid_m = mockito::mock("POST", "/ping/0")
            .match_query(mockito::Matcher::Regex("rid=.*".into()))
            .match_body("run id")
            .with_status(200).create();
        let agent = HCAgent{ agent: Agent::new(), verbose: false, url_prefix: format!("{}/{}", mockito::server_url(), "ping") };
        let suc_response = agent.notify_complete(None, Some(0), "foo bar");
        let fail_response = agent.notify_complete(None, Some(10), "bar baz");
        let log_response = agent.notify_complete(None, None, "bang boom");
        let runid_response = agent.notify_complete(Some(Uuid::from_u128(1234)), Some(0), "run id");
        suc_m.assert();
        fail_m.assert();
        log_m.assert();
        runid_m.assert();
        suc_response.unwrap();
        fail_response.unwrap();
        log_response.unwrap();
        runid_response.unwrap();
    }

    mod integ {
        use super::*;

        fn fake_cli(uuid: &str, command: &[&str]) -> Cli {
            Cli {
                uuid: Some(uuid.into()),
                slug: None,
                ping_key: None,
                time: false,
                head: false,
                ping_only: false,
                log: false,
                detailed: false,
                env: false,
                verbose: false,
                user_agent: None,
                base_url: mockito::server_url(),
                command: command.iter().map(OsString::from).collect(),
            }
        }

        #[test]
        fn success() {
            let m = mockito::mock("POST", "/success/0").match_body("hello\n").with_status(200).create();

            let cli = fake_cli("success", &["echo", "hello"]);
            let agent = HCAgent::create(&cli);
            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }

        #[test]
        fn fail() {
            let m = mockito::mock("POST", "/fail/5")
                .match_body("failed\n").with_status(200).create();

            let cli = fake_cli("fail", &["bash", "-c", "echo failed >&2; exit 5"]);
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }

        #[test]
        fn start() {
            let m = mockito::mock("GET", "/start/start")
                .match_query(mockito::Matcher::Regex("rid=.*".into()))
                .with_status(200).create();

            let cli = fake_cli("start", &[""]);

            let response = HCAgent::create(&cli).notify_start(Uuid::from_u128(1234));
            m.assert();
            response.unwrap();
        }

        #[test]
        fn log() {
            let m = mockito::mock("POST", "/log/log")
                .match_body("hello\n").with_status(200).create();

            let mut cli = fake_cli("log", &["echo", "hello"]);
            cli.log = true;
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }

        #[test]
        fn slug() {
            let m = mockito::mock("POST", "/key/slug/0")
                .match_body("hello\n").with_status(200).create();

            let mut cli = fake_cli("dont-use", &["echo", "hello"]);
            cli.uuid = None;
            cli.ping_key = Some("key".into());
            cli.slug = Some("slug".into());
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }

        #[test]
        fn unreachable() {
            // Unused, but necessary to isolate separate tests, per lipanski/mockito#111
            let m = mockito::mock("GET", "/").with_status(500).create();

            let cli = fake_cli("unreachable", &["true"]);
            let agent = HCAgent::create(&cli);

            run(cli, agent).expect_err("Should fail.");
            m.expect(0);
        }

        #[test]
        fn timed() {
            let start_m = mockito::mock("GET", "/timed/start")
                .match_query(mockito::Matcher::Regex("rid=.*".into()))
                .with_status(200).create();
            let done_m = mockito::mock("POST", "/timed/0")
                .match_query(mockito::Matcher::Regex("rid=.*".into()))
                .match_body("hello\n")
                .with_status(200).create();

            let mut cli = fake_cli("timed", &["echo", "hello"]);
            cli.time = true;
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            start_m.assert();
            done_m.assert();
            res.unwrap();
        }

        #[test]
        fn long_output() {
            use mockito::Matcher;
            let part = "🇺🇸⚾ ";
            let msg = part.repeat(1000);
            assert!(msg.len() > MAX_BYTES_TO_POST);
            assert!(!msg.is_char_boundary(msg.len()-MAX_BYTES_TO_POST-1));

            let m = mockito::mock("POST", "/long_output/0")
                .match_header("content-length", "9998")
                .match_body(Matcher::AllOf(vec!(
                    Matcher::Regex(format!("^ {}", part)),
                    Matcher::Regex(format!("{}\n$", part))
                )))
                .with_status(200).create();

            let cli = fake_cli("long_output", &["echo", &msg]);
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }

        #[test]
        fn quiet() {
            let m = mockito::mock("POST", "/quiet/0")
                .match_body(mockito::Matcher::Missing).with_status(200).create();

            let mut cli = fake_cli("quiet", &["echo", "quiet!"]);
            cli.ping_only = true;
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }

        #[test] fn detailed() {
            let m = mockito::mock("POST", "/detailed/0")
                .match_body(mockito::Matcher::Regex(
                    "^\\$ echo hello 2>&1\nhello\n\n\nExit Code: 0\nDuration: .*$".to_string()))
                .with_status(200).create();

            let mut cli = fake_cli("detailed", &["echo", "hello"]);
            cli.detailed = true;
            let agent = HCAgent::create(&cli);

            let res = run(cli, agent);
            m.assert();
            res.unwrap();
        }
    }
}
