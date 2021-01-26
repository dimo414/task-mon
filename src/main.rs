#[macro_use] extern crate clap;

#[cfg(test)] extern crate parameterized_test;

use std::convert::TryFrom;
use clap::{Arg, App};
use reqwest::blocking::{Client, Response};
use subprocess::{Exec, Redirection, ExitStatus, CaptureData, PopenConfig};
use std::ffi::OsStr;
use std::time::{Duration, Instant};
use reqwest::Error;

static MAX_BYTES_TO_POST: usize = 10000; // not 10KB, https://healthchecks.io/docs/attaching_logs/
static MAX_STRING_TO_LOG: usize = 1000;

/// Truncates a string for display
fn truncate_str(s: String, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", s.chars().take(max_len-3).collect::<String>())
    } else { s }
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

/// Constructs a User Agent string for requests to Healthchecks
fn make_user_agent(custom: Option<&str>) -> String {
    // Could use https://crates.io/crates/hostname or another crate to get more details
    let base = match std::env::var("HOSTNAME").ok() {
        Some(host) => format!("{} - {}", crate_name!(), host),
        None => crate_name!().to_string(),
    };

    match custom {
        Some(agent) => format!("{} ({})", agent, base),
        None => base,
    }
}

/// Pings the Healthchecks server to notify that the task denoted by the UUID is starting
fn notify_start(client: &Client, verbose: bool, base_url: &str, uuid: &str) ->  Result<Response, Error> {
    let req = client.get(&format!("{}/{}/start", base_url, uuid));
    if verbose { eprintln!("Sending request: {:?}", req); }
    req.send()?.error_for_status()
}

/// Pings the Healthchecks server to notify that the task denoted by the UUID is done.
/// If code is non-zero, the task will be considered failed.
fn notify_complete(client: &Client, verbose: bool, base_url: &str, uuid: &str, code: u8, output: &str) -> Result<Response, Error> {
    let mut req = client.post(&format!("{}/{}/{}", base_url, uuid, code));
    if !output.is_empty() {
        req = req.body(output.bytes().collect::<Vec<_>>());
    }
    if verbose { eprintln!("Sending request: {:?}", req); }
    req.send()?.error_for_status()
}

struct AppState<'a> {
    uuid: &'a str,
    time: bool,
    tail: bool,
    capture_output: bool,
    detailed: bool,
    env: bool,
    verbose: bool,
    base_url: &'a str,
    command: Vec<&'a str>,
}

fn run(state: AppState, http_client: Client) -> Result<Response, Error> {
    if state.time {
        if let Err(e) = notify_start(&http_client, state.verbose, state.base_url, state.uuid) {
            eprintln!("Failed to send start request: {:?}", e);
        }
    }
    let (mut output, code, elapsed) = execute(&state.command, state.capture_output, state.verbose);

    if state.detailed {
        // We could properly escape command, e.g. with https://crates.io/crates/shell-quote
        output = format!("$ {} 2>&1\n{}\n\nExit Code: {}\nDuration: {:?}",
                         state.command.join(" "), output, code, elapsed);
        if state.env {
            let env_str = PopenConfig::current_env().iter()
                .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
                .collect::<Vec<_>>().join("\n");
            output = format!("{}\n{}", env_str, output);
        }
    }

    // If we have too much output safely convert the last 10k bytes into UTF-8
    let output =
        if state.tail && output.len() > MAX_BYTES_TO_POST {
            String::from_utf8_lossy(&output.as_bytes()[output.len() - MAX_BYTES_TO_POST..])
        } else { std::borrow::Cow::Owned(output) };

    // Trim replacement chars added by from_utf8_lossy since they are multi-byte and can actually
    // increase the length of the string.
    notify_complete(&http_client, state.verbose, state.base_url, state.uuid, code, &output.trim_start_matches(|c| c=='�'))
}

fn main() {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about(crate_description!())
        .arg(Arg::with_name("uuid")
            .long("uuid")
            .short("k")
            .value_name("UUID")
            .required(true)
            .help("Healthchecks.io UUID to ping")
            .takes_value(true))
        .arg(Arg::with_name("time")
            .long("time")
            .short("t")
            .help("Ping when the program starts as well as completes"))
        .arg(Arg::with_name("head")
            .long("head")
            .help("POST the first 10k bytes instead of the last"))
        .arg(Arg::with_name("ping_only")
            .long("ping_only")
            .help("Don't POST any output from the command"))
        .arg(Arg::with_name("detailed")
            .long("detailed")
            .help("Include execution details in the information POST-ed (by default just sends stdout/err)"))
        .arg(Arg::with_name("env")
            .long("env")
            .requires("detailed")
            .help("Also POSTs the process environment; requires --detailed"))
        .arg(Arg::with_name("verbose")
            .long("verbose")
            .help("Write debugging details to stderr"))
        .arg(Arg::with_name("user_agent")
            .long("user_agent")
            .help("Customize the user-agent string sent to the Healthchecks.io server"))
        .arg(Arg::with_name("base_url")
            .long("base_url")
            .default_value("https://hc-ping.com")
            .help("Base URL of the Healthchecks.io server to ping"))
        .arg(Arg::with_name("command")
            .required(true)
            .multiple(true)
            .last(true)
            .help("The command to run"))
        .get_matches();

    let state = AppState {
        uuid: matches.value_of("uuid").expect("Required"),
        time: matches.is_present("time"),
        tail: !matches.is_present("head"),
        capture_output: !matches.is_present("ping_only"),
        detailed: matches.is_present("detailed"),
        env: matches.is_present("env"),
        verbose: matches.is_present("verbose"),
        base_url: matches.value_of("base_url").expect("Has default"),
        command: matches.values_of("command").expect("Required").collect(),
    };

    // TODO unit test against a mock/fake client: https://github.com/seanmonstar/reqwest/issues/154
    // TODO support retries: https://github.com/seanmonstar/reqwest/issues/316
    // TODO could potentially shrink the binary size by manually constructing requests with
    // https://doc.rust-lang.org/std/net/struct.TcpStream.html and https://docs.rs/native-tls/
    let http_client = Client::builder()
        .timeout(Duration::from_secs(10)) // https://healthchecks.io/docs/reliability_tips/
        .user_agent(make_user_agent(matches.value_of("user_agent")))
        .build().expect("http_client");

    run(state, http_client).expect("Failed to reach Healthchecks.io");
}

#[cfg(test)]
mod tests {
    use super::*;

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
        std::env::set_var("HOSTNAME", "host");
        assert_eq!(make_user_agent(None), format!("{} - host", crate_name!()));
        assert_eq!(make_user_agent(Some("foo")), format!("foo ({} - host)", crate_name!()));
        std::env::remove_var("HOSTNAME");
        assert_eq!(make_user_agent(None), crate_name!());
        assert_eq!(make_user_agent(Some("foo")), format!("foo ({})", crate_name!()));
    }

    #[test]
    fn start() {
        let m = mockito::mock("GET", "/start/start").with_status(200).create();
        let response = notify_start(&Client::new(), false, &mockito::server_url(), "start");
        m.assert();
        response.unwrap();
    }

    #[test]
    fn ping() {
        let suc_m = mockito::mock("POST", "/ping/0").match_body("foo bar").with_status(200).create();
        let fail_m = mockito::mock("POST", "/ping/10").match_body("bar baz").with_status(200).create();
        let suc_response = notify_complete(&Client::new(), false, &mockito::server_url(), "ping",0, "foo bar");
        let fail_response = notify_complete(&Client::new(), false, &mockito::server_url(), "ping",10, "bar baz");
        suc_m.assert();
        fail_m.assert();
        suc_response.unwrap();
        fail_response.unwrap();
    }

    mod integ {
        use super::*;

        fn state<'a>(base_url: &'a str, uuid: &'a str, command: Vec<&'a str>) -> AppState<'a> {
            AppState {
                uuid,
                time: false,
                tail: true,
                capture_output: true,
                detailed: false,
                env: false,
                verbose: false,
                base_url,
                command,
            }
        }

        #[test]
        fn success() {
            let m = mockito::mock("POST", "/success/0").match_body("hello\n").with_status(200).create();

            let server = mockito::server_url();
            let s = state(&server, "success", vec!("echo", "hello"));
            let res = run(s, Client::new());
            m.assert();
            res.unwrap();
        }

        #[test]
        fn fail() {
            let m = mockito::mock("POST", "/fail/5")
                .match_body("failed\n").with_status(200).create();

            let server = mockito::server_url();
            let s = state(&server, "fail", vec!("bash", "-c", "echo failed >&2; exit 5"));

            let res = run(s, Client::new());
            m.assert();
            res.unwrap();
        }

        #[test]
        fn unreachable() {
            // Unused, but necessary to isolate separate tests, per lipanski/mockito#111
            let m = mockito::mock("GET", "/").with_status(500).create();

            let server = mockito::server_url();
            let s = state(&server, "unreachable", vec!("true"));

            run(s, Client::new()).expect_err("Should fail.");
            m.expect(0);
        }

        #[test]
        fn timed() {
            let start_m = mockito::mock("GET", "/timed/start").with_status(200).create();
            let done_m = mockito::mock("POST", "/timed/0")
                .match_body("hello\n").with_status(200).create();

            let server = mockito::server_url();
            let mut s = state(&server, "timed", vec!("echo", "hello"));
            s.time = true;

            let res = run(s, Client::new());
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

            let server = mockito::server_url();
            let s = state(&server, "long_output", vec!("echo", &msg));

            let res = run(s, Client::new());
            m.assert();
            res.unwrap();
        }

        #[test]
        fn quiet() {
            let m = mockito::mock("POST", "/quiet/0")
                .match_body(mockito::Matcher::Missing).with_status(200).create();

            let server = mockito::server_url();
            let mut s = state(&server, "quiet", vec!("echo", "quiet!"));
            s.capture_output = false;

            let res = run(s, Client::new());
            m.assert();
            res.unwrap();
        }

        #[test] fn detailed() {
            let m = mockito::mock("POST", "/detailed/0")
                .match_body(mockito::Matcher::Regex(
                    "^\\$ echo hello 2>&1\nhello\n\n\nExit Code: 0\nDuration: .*$".to_string()))
                .with_status(200).create();

            let server = mockito::server_url();
            let mut s = state(&server, "detailed", vec!("echo", "hello"));
            s.detailed = true;

            let res = run(s, Client::new());
            m.assert();
            res.unwrap();
        }
    }
}
