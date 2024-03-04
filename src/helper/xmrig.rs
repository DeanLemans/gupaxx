use crate::helper::{ProcessName, ProcessSignal, ProcessState};
use crate::regex::XMRIG_REGEX;
use crate::utils::sudo::SudoState;
use crate::{constants::*, human::*, macros::*};
use log::*;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::{
    fmt::Write,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    thread,
    time::*,
};

use super::{Helper, Process};
impl Helper {
    #[cold]
    #[inline(never)]
    fn read_pty_xmrig(
        output_parse: Arc<Mutex<String>>,
        output_pub: Arc<Mutex<String>>,
        reader: Box<dyn std::io::Read + Send>,
    ) {
        use std::io::BufRead;
        let mut stdout = std::io::BufReader::new(reader).lines();

        // Run a ANSI escape sequence filter for the first few lines.
        let mut i = 0;
        while let Some(Ok(line)) = stdout.next() {
            let line = strip_ansi_escapes::strip_str(line);
            if let Err(e) = writeln!(lock!(output_parse), "{}", line) {
                error!("XMRig PTY Parse | Output error: {}", e);
            }
            if let Err(e) = writeln!(lock!(output_pub), "{}", line) {
                error!("XMRig PTY Pub | Output error: {}", e);
            }
            if i > 20 {
                break;
            } else {
                i += 1;
            }
        }

        while let Some(Ok(line)) = stdout.next() {
            //			println!("{}", line); // For debugging.
            if let Err(e) = writeln!(lock!(output_parse), "{}", line) {
                error!("XMRig PTY Parse | Output error: {}", e);
            }
            if let Err(e) = writeln!(lock!(output_pub), "{}", line) {
                error!("XMRig PTY Pub | Output error: {}", e);
            }
        }
    }
    //---------------------------------------------------------------------------------------------------- XMRig specific, most functions are very similar to P2Pool's
    #[cold]
    #[inline(never)]
    // If processes are started with [sudo] on macOS, they must also
    // be killed with [sudo] (even if I have a direct handle to it as the
    // parent process...!). This is only needed on macOS, not Linux.
    fn sudo_kill(pid: u32, sudo: &Arc<Mutex<SudoState>>) -> bool {
        // Spawn [sudo] to execute [kill] on the given [pid]
        let mut child = std::process::Command::new("sudo")
            .args(["--stdin", "kill", "-9", &pid.to_string()])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();

        // Write the [sudo] password to STDIN.
        let mut stdin = child.stdin.take().unwrap();
        use std::io::Write;
        if let Err(e) = writeln!(stdin, "{}\n", lock!(sudo).pass) {
            error!("Sudo Kill | STDIN error: {}", e);
        }

        // Return exit code of [sudo/kill].
        child.wait().unwrap().success()
    }

    #[cold]
    #[inline(never)]
    // Just sets some signals for the watchdog thread to pick up on.
    pub fn stop_xmrig(helper: &Arc<Mutex<Self>>) {
        info!("XMRig | Attempting to stop...");
        lock2!(helper, xmrig).signal = ProcessSignal::Stop;
        lock2!(helper, xmrig).state = ProcessState::Middle;
    }

    #[cold]
    #[inline(never)]
    // The "restart frontend" to a "frontend" function.
    // Basically calls to kill the current xmrig, waits a little, then starts the below function in a a new thread, then exit.
    pub fn restart_xmrig(
        helper: &Arc<Mutex<Self>>,
        state: &crate::disk::state::Xmrig,
        path: &Path,
        sudo: Arc<Mutex<SudoState>>,
    ) {
        info!("XMRig | Attempting to restart...");
        lock2!(helper, xmrig).signal = ProcessSignal::Restart;
        lock2!(helper, xmrig).state = ProcessState::Middle;

        let helper = Arc::clone(helper);
        let state = state.clone();
        let path = path.to_path_buf();
        // This thread lives to wait, start xmrig then die.
        thread::spawn(move || {
            while lock2!(helper, xmrig).state != ProcessState::Waiting {
                warn!("XMRig | Want to restart but process is still alive, waiting...");
                sleep!(1000);
            }
            // Ok, process is not alive, start the new one!
            info!("XMRig | Old process seems dead, starting new one!");
            Self::start_xmrig(&helper, &state, &path, sudo);
        });
        info!("XMRig | Restart ... OK");
    }

    #[cold]
    #[inline(never)]
    pub fn start_xmrig(
        helper: &Arc<Mutex<Self>>,
        state: &crate::disk::state::Xmrig,
        path: &Path,
        sudo: Arc<Mutex<SudoState>>,
    ) {
        lock2!(helper, xmrig).state = ProcessState::Middle;

        let (args, api_ip_port) = Self::build_xmrig_args_and_mutate_img(helper, state, path);

        // Print arguments & user settings to console
        crate::disk::print_dash(&format!("XMRig | Launch arguments: {:#?}", args));
        info!("XMRig | Using path: [{}]", path.display());

        // Spawn watchdog thread
        let process = Arc::clone(&lock!(helper).xmrig);
        let gui_api = Arc::clone(&lock!(helper).gui_api_xmrig);
        let pub_api = Arc::clone(&lock!(helper).pub_api_xmrig);
        let path = path.to_path_buf();
        thread::spawn(move || {
            Self::spawn_xmrig_watchdog(process, gui_api, pub_api, args, path, sudo, api_ip_port);
        });
    }

    #[cold]
    #[inline(never)]
    // Takes in some [State/Xmrig] and parses it to build the actual command arguments.
    // Returns the [Vec] of actual arguments, and mutates the [ImgXmrig] for the main GUI thread
    // It returns a value... and mutates a deeply nested passed argument... this is some pretty bad code...
    pub fn build_xmrig_args_and_mutate_img(
        helper: &Arc<Mutex<Self>>,
        state: &crate::disk::state::Xmrig,
        path: &std::path::Path,
    ) -> (Vec<String>, String) {
        let mut args = Vec::with_capacity(500);
        let mut api_ip = String::with_capacity(15);
        let mut api_port = String::with_capacity(5);
        let path = path.to_path_buf();
        // The actual binary we're executing is [sudo], technically
        // the XMRig path is just an argument to sudo, so add it.
        // Before that though, add the ["--prompt"] flag and set it
        // to emptyness so that it doesn't show up in the output.
        if cfg!(unix) {
            args.push(r#"--prompt="#.to_string());
            args.push("--".to_string());
            args.push(path.display().to_string());
        }

        // [Simple]
        if state.simple {
            // Build the xmrig argument
            let rig = if state.simple_rig.is_empty() {
                GUPAX_VERSION_UNDERSCORE.to_string()
            } else {
                state.simple_rig.clone()
            }; // Rig name
            args.push("--url".to_string());
            args.push("127.0.0.1:3333".to_string()); // Local P2Pool (the default)
            args.push("--threads".to_string());
            args.push(state.current_threads.to_string()); // Threads
            args.push("--user".to_string());
            args.push(rig); // Rig name
            args.push("--no-color".to_string()); // No color
            args.push("--http-host".to_string());
            args.push("127.0.0.1".to_string()); // HTTP API IP
            args.push("--http-port".to_string());
            args.push("18088".to_string()); // HTTP API Port
            if state.pause != 0 {
                args.push("--pause-on-active".to_string());
                args.push(state.pause.to_string());
            } // Pause on active
            *lock2!(helper, img_xmrig) = ImgXmrig {
                threads: state.current_threads.to_string(),
                url: "127.0.0.1:3333 (Local P2Pool)".to_string(),
            };
            api_ip = "127.0.0.1".to_string();
            api_port = "18088".to_string();

        // [Advanced]
        } else {
            // Overriding command arguments
            if !state.arguments.is_empty() {
                // This parses the input and attempts to fill out
                // the [ImgXmrig]... This is pretty bad code...
                let mut last = "";
                let lock = lock!(helper);
                let mut xmrig_image = lock!(lock.img_xmrig);
                for arg in state.arguments.split_whitespace() {
                    match last {
                        "--threads" => xmrig_image.threads = arg.to_string(),
                        "--url" => xmrig_image.url = arg.to_string(),
                        "--http-host" => {
                            api_ip = if arg == "localhost" {
                                "127.0.0.1".to_string()
                            } else {
                                arg.to_string()
                            }
                        }
                        "--http-port" => api_port = arg.to_string(),
                        _ => (),
                    }
                    args.push(if arg == "localhost" {
                        "127.0.0.1".to_string()
                    } else {
                        arg.to_string()
                    });
                    last = arg;
                }
            // Else, build the argument
            } else {
                // XMRig doesn't understand [localhost]
                let ip = if state.ip == "localhost" || state.ip.is_empty() {
                    "127.0.0.1"
                } else {
                    &state.ip
                };
                api_ip = if state.api_ip == "localhost" || state.api_ip.is_empty() {
                    "127.0.0.1".to_string()
                } else {
                    state.api_ip.to_string()
                };
                api_port = if state.api_port.is_empty() {
                    "18088".to_string()
                } else {
                    state.api_port.to_string()
                };
                let url = format!("{}:{}", ip, state.port); // Combine IP:Port into one string
                args.push("--user".to_string());
                args.push(state.address.clone()); // Wallet
                args.push("--threads".to_string());
                args.push(state.current_threads.to_string()); // Threads
                args.push("--rig-id".to_string());
                args.push(state.rig.to_string()); // Rig ID
                args.push("--url".to_string());
                args.push(url.clone()); // IP/Port
                args.push("--http-host".to_string());
                args.push(api_ip.to_string()); // HTTP API IP
                args.push("--http-port".to_string());
                args.push(api_port.to_string()); // HTTP API Port
                args.push("--no-color".to_string()); // No color escape codes
                if state.tls {
                    args.push("--tls".to_string());
                } // TLS
                if state.keepalive {
                    args.push("--keepalive".to_string());
                } // Keepalive
                if state.pause != 0 {
                    args.push("--pause-on-active".to_string());
                    args.push(state.pause.to_string());
                } // Pause on active
                *lock2!(helper, img_xmrig) = ImgXmrig {
                    url,
                    threads: state.current_threads.to_string(),
                };
            }
        }
        (args, format!("{}:{}", api_ip, api_port))
    }

    // We actually spawn [sudo] on Unix, with XMRig being the argument.
    #[cfg(target_family = "unix")]
    fn create_xmrig_cmd_unix(args: Vec<String>, path: PathBuf) -> portable_pty::CommandBuilder {
        let mut cmd = portable_pty::cmdbuilder::CommandBuilder::new("sudo");
        cmd.args(args);
        cmd.cwd(path.as_path().parent().unwrap());
        cmd
    }

    // Gupax should be admin on Windows, so just spawn XMRig normally.
    #[cfg(target_os = "windows")]
    fn create_xmrig_cmd_windows(args: Vec<String>, path: PathBuf) -> portable_pty::CommandBuilder {
        let mut cmd = portable_pty::cmdbuilder::CommandBuilder::new(path.clone());
        cmd.args(args);
        cmd.cwd(path.as_path().parent().unwrap());
        cmd
    }

    #[cold]
    #[inline(never)]
    // The XMRig watchdog. Spawns 1 OS thread for reading a PTY (STDOUT+STDERR), and combines the [Child] with a PTY so STDIN actually works.
    // This isn't actually async, a tokio runtime is unfortunately needed because [Hyper] is an async library (HTTP API calls)
    #[tokio::main]
    #[allow(clippy::await_holding_lock)]
    async fn spawn_xmrig_watchdog(
        process: Arc<Mutex<Process>>,
        gui_api: Arc<Mutex<PubXmrigApi>>,
        pub_api: Arc<Mutex<PubXmrigApi>>,
        args: Vec<String>,
        path: std::path::PathBuf,
        sudo: Arc<Mutex<SudoState>>,
        mut api_ip_port: String,
    ) {
        // 1a. Create PTY
        debug!("XMRig | Creating PTY...");
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(portable_pty::PtySize {
                rows: 100,
                cols: 1000,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        // 1b. Create command
        debug!("XMRig | Creating command...");
        #[cfg(target_os = "windows")]
        let cmd = Self::create_xmrig_cmd_windows(args, path);
        #[cfg(target_family = "unix")]
        let cmd = Self::create_xmrig_cmd_unix(args, path);
        // 1c. Create child
        debug!("XMRig | Creating child...");
        let child_pty = arc_mut!(pair.slave.spawn_command(cmd).unwrap());
        drop(pair.slave);

        let mut stdin = pair.master.take_writer().unwrap();

        // 2. Input [sudo] pass, wipe, then drop.
        if cfg!(unix) {
            debug!("XMRig | Inputting [sudo] and wiping...");
            // a) Sleep to wait for [sudo]'s non-echo prompt (on Unix).
            // this prevents users pass from showing up in the STDOUT.
            sleep!(3000);
            if let Err(e) = writeln!(stdin, "{}", lock!(sudo).pass) {
                error!("XMRig | Sudo STDIN error: {}", e);
            };
            SudoState::wipe(&sudo);

            // b) Reset GUI STDOUT just in case.
            debug!("XMRig | Clearing GUI output...");
            lock!(gui_api).output.clear();
        }

        // 3. Set process state
        debug!("XMRig | Setting process state...");
        let mut lock = lock!(process);
        lock.state = ProcessState::NotMining;
        lock.signal = ProcessSignal::None;
        lock.start = Instant::now();
        let reader = pair.master.try_clone_reader().unwrap(); // Get STDOUT/STDERR before moving the PTY
        drop(lock);

        // 4. Spawn PTY read thread
        debug!("XMRig | Spawning PTY read thread...");
        let output_parse = Arc::clone(&lock!(process).output_parse);
        let output_pub = Arc::clone(&lock!(process).output_pub);
        thread::spawn(move || {
            Self::read_pty_xmrig(output_parse, output_pub, reader);
        });
        let output_parse = Arc::clone(&lock!(process).output_parse);
        let output_pub = Arc::clone(&lock!(process).output_pub);

        let client: hyper::Client<hyper::client::HttpConnector> =
            hyper::Client::builder().build(hyper::client::HttpConnector::new());
        let start = lock!(process).start;
        let api_uri = {
            if !api_ip_port.ends_with('/') {
                api_ip_port.push('/');
            }
            "http://".to_owned() + &api_ip_port + XMRIG_API_URI
        };
        info!("XMRig | Final API URI: {}", api_uri);

        // Reset stats before loop
        *lock!(pub_api) = PubXmrigApi::new();
        *lock!(gui_api) = PubXmrigApi::new();

        // 5. Loop as watchdog
        info!("XMRig | Entering watchdog mode... woof!");
        loop {
            // Set timer
            let now = Instant::now();
            debug!("XMRig Watchdog | ----------- Start of loop -----------");

            // Check if the process secretly died without us knowing :)
            if let Ok(Some(code)) = lock!(child_pty).try_wait() {
                debug!("XMRig Watchdog | Process secretly died on us! Getting exit status...");
                let exit_status = match code.success() {
                    true => {
                        lock!(process).state = ProcessState::Dead;
                        "Successful"
                    }
                    false => {
                        lock!(process).state = ProcessState::Failed;
                        "Failed"
                    }
                };
                let uptime = HumanTime::into_human(start.elapsed());
                info!(
                    "XMRig | Stopped ... Uptime was: [{}], Exit status: [{}]",
                    uptime, exit_status
                );
                if let Err(e) = writeln!(
                    lock!(gui_api).output,
                    "{}\nXMRig stopped | Uptime: [{}] | Exit status: [{}]\n{}\n\n\n\n",
                    HORI_CONSOLE,
                    uptime,
                    exit_status,
                    HORI_CONSOLE
                ) {
                    error!(
                        "XMRig Watchdog | GUI Uptime/Exit status write failed: {}",
                        e
                    );
                }
                lock!(process).signal = ProcessSignal::None;
                debug!("XMRig Watchdog | Secret dead process reap OK, breaking");
                break;
            }

            // Stop on [Stop/Restart] SIGNAL
            let signal = lock!(process).signal;
            if signal == ProcessSignal::Stop || signal == ProcessSignal::Restart {
                debug!("XMRig Watchdog | Stop/Restart SIGNAL caught");
                // macOS requires [sudo] again to kill [XMRig]
                if cfg!(target_os = "macos") {
                    // If we're at this point, that means the user has
                    // entered their [sudo] pass again, after we wiped it.
                    // So, we should be able to find it in our [Arc<Mutex<SudoState>>].
                    Self::sudo_kill(lock!(child_pty).process_id().unwrap(), &sudo);
                    // And... wipe it again (only if we're stopping full).
                    // If we're restarting, the next start will wipe it for us.
                    if signal != ProcessSignal::Restart {
                        SudoState::wipe(&sudo);
                    }
                } else if let Err(e) = lock!(child_pty).kill() {
                    error!("XMRig Watchdog | Kill error: {}", e);
                }
                let exit_status = match lock!(child_pty).wait() {
                    Ok(e) => {
                        let mut process = lock!(process);
                        if e.success() {
                            if process.signal == ProcessSignal::Stop {
                                process.state = ProcessState::Dead;
                            }
                            "Successful"
                        } else {
                            if process.signal == ProcessSignal::Stop {
                                process.state = ProcessState::Failed;
                            }
                            "Failed"
                        }
                    }
                    _ => {
                        let mut process = lock!(process);
                        if process.signal == ProcessSignal::Stop {
                            process.state = ProcessState::Failed;
                        }
                        "Unknown Error"
                    }
                };
                let uptime = HumanTime::into_human(start.elapsed());
                info!(
                    "XMRig | Stopped ... Uptime was: [{}], Exit status: [{}]",
                    uptime, exit_status
                );
                if let Err(e) = writeln!(
                    lock!(gui_api).output,
                    "{}\nXMRig stopped | Uptime: [{}] | Exit status: [{}]\n{}\n\n\n\n",
                    HORI_CONSOLE,
                    uptime,
                    exit_status,
                    HORI_CONSOLE
                ) {
                    error!(
                        "XMRig Watchdog | GUI Uptime/Exit status write failed: {}",
                        e
                    );
                }
                let mut process = lock!(process);
                match process.signal {
                    ProcessSignal::Stop => process.signal = ProcessSignal::None,
                    ProcessSignal::Restart => process.state = ProcessState::Waiting,
                    _ => (),
                }
                debug!("XMRig Watchdog | Stop/Restart SIGNAL done, breaking");
                break;
            }

            // Check vector of user input
            {
                let mut lock = lock!(process);
                if !lock.input.is_empty() {
                    let input = std::mem::take(&mut lock.input);
                    for line in input {
                        if line.is_empty() {
                            continue;
                        }
                        debug!(
                            "XMRig Watchdog | User input not empty, writing to STDIN: [{}]",
                            line
                        );
                        #[cfg(target_os = "windows")]
                        if let Err(e) = write!(stdin, "{}\r\n", line) {
                            error!("XMRig Watchdog | STDIN error: {}", e);
                        }
                        #[cfg(target_family = "unix")]
                        if let Err(e) = writeln!(stdin, "{}", line) {
                            error!("XMRig Watchdog | STDIN error: {}", e);
                        }
                        // Flush.
                        if let Err(e) = stdin.flush() {
                            error!("XMRig Watchdog | STDIN flush error: {}", e);
                        }
                    }
                }
            }
            // Check if logs need resetting
            debug!("XMRig Watchdog | Attempting GUI log reset check");
            {
                let mut lock = lock!(gui_api);
                Self::check_reset_gui_output(&mut lock.output, ProcessName::Xmrig);
            }
            // Always update from output
            debug!("XMRig Watchdog | Starting [update_from_output()]");
            PubXmrigApi::update_from_output(
                &pub_api,
                &output_pub,
                &output_parse,
                start.elapsed(),
                &process,
            );

            // Send an HTTP API request
            debug!("XMRig Watchdog | Attempting HTTP API request...");
            if let Ok(priv_api) = PrivXmrigApi::request_xmrig_api(client.clone(), &api_uri).await {
                debug!("XMRig Watchdog | HTTP API request OK, attempting [update_from_priv()]");
                PubXmrigApi::update_from_priv(&pub_api, priv_api);
            } else {
                warn!(
                    "XMRig Watchdog | Could not send HTTP API request to: {}",
                    api_uri
                );
            }

            // Sleep (only if 900ms hasn't passed)
            let elapsed = now.elapsed().as_millis();
            // Since logic goes off if less than 1000, casting should be safe
            if elapsed < 900 {
                let sleep = (900 - elapsed) as u64;
                debug!(
                    "XMRig Watchdog | END OF LOOP - Sleeping for [{}]ms...",
                    sleep
                );
                sleep!(sleep);
            } else {
                debug!("XMRig Watchdog | END OF LOOP - Not sleeping!");
            }
        }

        // 5. If loop broke, we must be done here.
        info!("XMRig Watchdog | Watchdog thread exiting... Goodbye!");
    }
}
//---------------------------------------------------------------------------------------------------- [ImgXmrig]
#[derive(Debug, Clone)]
pub struct ImgXmrig {
    pub threads: String,
    pub url: String,
}

impl Default for ImgXmrig {
    fn default() -> Self {
        Self::new()
    }
}

impl ImgXmrig {
    pub fn new() -> Self {
        Self {
            threads: "???".to_string(),
            url: "???".to_string(),
        }
    }
}

//---------------------------------------------------------------------------------------------------- Public XMRig API
#[derive(Debug, Clone)]
pub struct PubXmrigApi {
    pub output: String,
    pub uptime: HumanTime,
    pub worker_id: String,
    pub resources: HumanNumber,
    pub hashrate: HumanNumber,
    pub diff: HumanNumber,
    pub accepted: HumanNumber,
    pub rejected: HumanNumber,

    pub hashrate_raw: f32,
}

impl Default for PubXmrigApi {
    fn default() -> Self {
        Self::new()
    }
}

impl PubXmrigApi {
    pub fn new() -> Self {
        Self {
            output: String::new(),
            uptime: HumanTime::new(),
            worker_id: "???".to_string(),
            resources: HumanNumber::unknown(),
            hashrate: HumanNumber::unknown(),
            diff: HumanNumber::unknown(),
            accepted: HumanNumber::unknown(),
            rejected: HumanNumber::unknown(),
            hashrate_raw: 0.0,
        }
    }

    #[inline]
    pub(super) fn combine_gui_pub_api(gui_api: &mut Self, pub_api: &mut Self) {
        let output = std::mem::take(&mut gui_api.output);
        let buf = std::mem::take(&mut pub_api.output);
        *gui_api = Self {
            output,
            ..std::mem::take(pub_api)
        };
        if !buf.is_empty() {
            gui_api.output.push_str(&buf);
        }
    }

    // This combines the buffer from the PTY thread [output_pub]
    // with the actual [PubApiXmrig] output field.
    pub(super) fn update_from_output(
        public: &Arc<Mutex<Self>>,
        output_parse: &Arc<Mutex<String>>,
        output_pub: &Arc<Mutex<String>>,
        elapsed: std::time::Duration,
        process: &Arc<Mutex<Process>>,
    ) {
        // 1. Take the process's current output buffer and combine it with Pub (if not empty)
        let mut output_pub = lock!(output_pub);

        {
            let mut public = lock!(public);
            if !output_pub.is_empty() {
                public.output.push_str(&std::mem::take(&mut *output_pub));
            }
            // Update uptime
            public.uptime = HumanTime::into_human(elapsed);
        }

        // 2. Check for "new job"/"no active...".
        let mut output_parse = lock!(output_parse);
        if XMRIG_REGEX.new_job.is_match(&output_parse) {
            lock!(process).state = ProcessState::Alive;
        } else if XMRIG_REGEX.not_mining.is_match(&output_parse) {
            lock!(process).state = ProcessState::NotMining;
        }

        // 3. Throw away [output_parse]
        output_parse.clear();
        drop(output_parse);
    }

    // Formats raw private data into ready-to-print human readable version.
    fn update_from_priv(public: &Arc<Mutex<Self>>, private: PrivXmrigApi) {
        let mut public = lock!(public);
        let hashrate_raw = match private.hashrate.total.first() {
            Some(Some(h)) => *h,
            _ => 0.0,
        };

        *public = Self {
            worker_id: private.worker_id,
            resources: HumanNumber::from_load(private.resources.load_average),
            hashrate: HumanNumber::from_hashrate(private.hashrate.total),
            diff: HumanNumber::from_u128(private.connection.diff),
            accepted: HumanNumber::from_u128(private.connection.accepted),
            rejected: HumanNumber::from_u128(private.connection.rejected),
            hashrate_raw,
            ..std::mem::take(&mut *public)
        }
    }
}

//---------------------------------------------------------------------------------------------------- Private XMRig API
// This matches to some JSON stats in the HTTP call [summary],
// e.g: [wget -qO- localhost:18085/1/summary].
// XMRig doesn't initialize stats at 0 (or 0.0) and instead opts for [null]
// which means some elements need to be wrapped in an [Option] or else serde will [panic!].
#[derive(Debug, Serialize, Deserialize, Clone)]
pub(super) struct PrivXmrigApi {
    worker_id: String,
    resources: Resources,
    connection: Connection,
    hashrate: Hashrate,
}

impl PrivXmrigApi {
    fn new() -> Self {
        Self {
            worker_id: String::new(),
            resources: Resources::new(),
            connection: Connection::new(),
            hashrate: Hashrate::new(),
        }
    }

    #[inline]
    // Send an HTTP request to XMRig's API, serialize it into [Self] and return it
    async fn request_xmrig_api(
        client: hyper::Client<hyper::client::HttpConnector>,
        api_uri: &str,
    ) -> std::result::Result<Self, anyhow::Error> {
        let request = hyper::Request::builder()
            .method("GET")
            .uri(api_uri)
            .body(hyper::Body::empty())?;
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            client.request(request),
        )
        .await?;
        let body = hyper::body::to_bytes(response?.body_mut()).await?;
        Ok(serde_json::from_slice::<Self>(&body)?)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
struct Resources {
    load_average: [Option<f32>; 3],
}
impl Resources {
    fn new() -> Self {
        Self {
            load_average: [Some(0.0), Some(0.0), Some(0.0)],
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Connection {
    diff: u128,
    accepted: u128,
    rejected: u128,
}
impl Connection {
    fn new() -> Self {
        Self {
            diff: 0,
            accepted: 0,
            rejected: 0,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
struct Hashrate {
    total: [Option<f32>; 3],
}
impl Hashrate {
    fn new() -> Self {
        Self {
            total: [Some(0.0), Some(0.0), Some(0.0)],
        }
    }
}