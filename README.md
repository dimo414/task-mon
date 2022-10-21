# Task Monitoring with Healthchecks.io

[![github](https://img.shields.io/badge/github-dimo414/task--mon-green?logo=github)](https://github.com/dimo414/task-mon)
[![crates.io](https://img.shields.io/crates/v/task-mon.svg?logo=rust)](https://crates.io/crates/task-mon)
[![build status](https://img.shields.io/github/workflow/status/dimo414/task-mon/Rust/master)](https://github.com/dimo414/task-mon/actions)
[![issues](https://img.shields.io/github/issues/dimo414/task-mon)](https://github.com/dimo414/task-mon/issues)
[![license](https://img.shields.io/github/license/dimo414/task-mon)](https://github.com/dimo414/task-mon/blob/master/LICENSE)


`task-mon` is a small binary for notifying Healthchecks.io when a command runs.

This serves a similar purpose to the `curl`-based patterns described in the Healthchecks
documentation but provides more flexibility and ergonomics. Especially for shell scripts and
[cron jobs](https://healthchecks.io/docs/monitoring_cron_jobs/), delegating health management to a
separate binary allows you to focus on the task at hand.

It supports Healthchecks' advanced optional features such as
[reporting failures](https://healthchecks.io/docs/signaling_failures/),
[attaching logs](https://healthchecks.io/docs/attaching_logs/), and
[monitoring execution time](https://healthchecks.io/docs/measuring_script_run_time/).

## Usage

To execute a task and ping Healthchecks.io when it completes simply invoke `task-mon` with the
check's UUID and the command to run:

```shell
$ task-mon --uuid 1234-abcd -- some_command --to --monitor
```

```shell
$ task-mon --ping-key abcd1234 --slug foo -- some_command --to --monitor
```

```shell
$ crontab -e
# m h dom mon dow command
  8 6 * * * /usr/local/cargo/bin/task-mon --uuid 1234-abcd -- some_command --to --monitor
```

`task-mon` will run the command and ping Healthchecks.io when it completes, reporting the exit
status and the last 10K of output from the process.

### Customization

```shell
$ task-mon --help
task-mon 0.3.0
CLI to execute commands and log results to healthchecks.io

USAGE:
    task-mon [OPTIONS] <--uuid <UUID>|--slug <SLUG>> [--] <COMMAND>...

ARGS:
    <COMMAND>...    The command to run

OPTIONS:
    -k, --uuid <UUID>                Check's UUID to ping
    -s, --slug <SLUG>                Check's slug name to ping, requires also specifying --ping-key
        --ping-key <PING_KEY>        Check's project ping key, required when using --slug [env:
                                     HEALTHCHECKS_PING_KEY=]
    -t, --time                       Ping when the program starts as well as completes
        --head                       POST the first 10k bytes instead of the last
        --ping-only                  Don't POST any output from the command
        --log                        Log the invocation without signalling success or failure; does
                                     not update the check's status
        --detailed                   Include execution details in the information POST-ed (by
                                     default just sends stdout/err
        --env                        Also POSTs the process environment; requires --detailed
        --verbose                    Write debugging details to stderr
        --user-agent <USER_AGENT>    Customize the user-agent string sent to the Healthchecks.io
                                     server
        --base-url <BASE_URL>        Base URL of the Healthchecks.io server to ping [env:
                                     HEALTHCHECKS_BASE_URL=] [default: https://hc-ping.com]
    -h, --help                       Print help information
    -V, --version                    Print version information
```

## Related projects

There are of course a number of similar projects out there, but I was bored and didn't want to use
any of them...

* [Runitor](https://github.com/bdd/runitor) - linked from the
  [Healthchecks docs](https://healthchecks.io/docs/attaching_logs/)
* [healthchecks-rs](https://github.com/msfjarvis/healthchecks-rs) - Rust library and CLI for pinging and
  monitoring Healthchecks
* [hchk](https://github.com/healthchecks/hchk) - older CLI written by the Healthchecks.io maintainer
