//! The fixture adapter binary.

use oxidepm_adapter_fixture::engine::Engine;
use oxidepm_adapter_fixture::report::Report;
use oxidepm_adapter_fixture::script::Script;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--child") {
        std::process::exit(oxidepm_adapter_fixture::child::child_main(&args[1..]));
    }
    if std::env::var_os("OXIDEPM_ADAPTER_PROTOCOLS").is_none() {
        eprintln!("this is an OxidePM adapter; it is started by oxidepmd");
        std::process::exit(1);
    }
    let script = match std::env::var_os("FIXTURE_SCRIPT") {
        None => Script::default(),
        Some(path) => match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| Script::parse(&t))
        {
            Ok(script) => script,
            Err(e) => {
                eprintln!(
                    "fixture: cannot use the script {}: {e}",
                    path.to_string_lossy()
                );
                std::process::exit(2);
            }
        },
    };
    let engine = Engine::new(script, Report::from_env());
    // a session verb: the terminal is standard input and output, so the daemon's frames come over
    // the socket named in the environment
    if let (Some(path), Some(token)) = (
        std::env::var_os("OXIDEPM_SESSION_SOCKET"),
        std::env::var("OXIDEPM_SESSION_TOKEN").ok(),
    ) {
        use std::io::Write as _;
        let hello = serde_json::json!({ "session": "adapter", "token": token }).to_string();
        let connected = std::os::unix::net::UnixStream::connect(&path).and_then(|mut stream| {
            stream.write_all(hello.as_bytes())?;
            stream.write_all(b"\n")?;
            Ok(stream)
        });
        match connected {
            Ok(stream) => engine.use_channel(stream),
            Err(e) => {
                eprintln!(
                    "fixture: cannot reach the session socket {}: {e}",
                    path.to_string_lossy()
                );
                std::process::exit(2);
            }
        }
    }
    engine.run()
}
