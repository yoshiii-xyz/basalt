use std::io::{self, BufReader, Write};
use std::time::Duration;

use basalt::{Database, cli};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--crash-test-writer") {
        crash_test_writer(args.get(1).map(String::as_str));
        return;
    }
    if args.first().map(String::as_str) == Some("mcp") {
        run_mcp(&args[1..]);
        return;
    }
    if args.first().map(String::as_str) == Some("workspace") {
        run_workspace(&args[1..]);
        return;
    }
    if args.first().map(String::as_str) == Some("init") {
        run_workspace(&args);
        return;
    }
    if args.first().map(String::as_str) == Some("--crash-test-workspace-apply") {
        crash_test_workspace_apply(&args[1..]);
        return;
    }
    if args.first().map(String::as_str) == Some("--crash-test-workspace-undo") {
        crash_test_workspace_undo(&args[1..]);
        return;
    }

    let options = match cli::parse_args(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("basalt: {error}");
            std::process::exit(2);
        }
    };
    if options.help {
        print!("{}", cli::HELP);
        return;
    }
    if options.version {
        println!("basalt {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let database = match open_database(&options.database) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("basalt: {error}");
            std::process::exit(1);
        }
    };

    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut output = io::stdout();
    if let Err(error) = cli::run(&options, database, &mut input, &mut output) {
        let _ = writeln!(io::stderr(), "basalt: {error}");
        std::process::exit(1);
    }
}

fn run_mcp(args: &[String]) {
    let options = match basalt::mcp::parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("basalt mcp: {error}");
            std::process::exit(2);
        }
    };
    if options.help {
        print!("{}", basalt::mcp::HELP);
        return;
    }

    let init_workspace = options.init_workspace;
    let result = if let Some(path) = options.workspace {
        let workspace = if init_workspace {
            basalt::workspace::Workspace::open_or_init(path)
        } else {
            basalt::workspace::Workspace::open(path)
        };
        match workspace {
            Ok(workspace) => basalt::mcp::run_workspace(workspace, options.allow_writes),
            Err(error) => Err(format!("could not open workspace: {error}")),
        }
    } else {
        match open_database(&options.database) {
            Ok(database) => basalt::mcp::run_database(database, options.allow_writes),
            Err(error) => Err(error.to_string()),
        }
    };

    if let Err(error) = result {
        eprintln!("basalt mcp: {error}");
        std::process::exit(1);
    }
}

fn run_workspace(args: &[String]) {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout();
    if let Err(error) = basalt::workspace::run(args, &mut input, &mut output) {
        eprintln!("basalt workspace: {error}");
        std::process::exit(error.exit_code());
    }
}

fn crash_test_workspace_apply(args: &[String]) {
    if args.len() != 2 {
        eprintln!("basalt: --crash-test-workspace-apply requires a workspace path and plan ID");
        std::process::exit(2);
    }
    run_workspace(&["apply".to_string(), args[0].clone(), args[1].clone()]);
}

fn crash_test_workspace_undo(args: &[String]) {
    if args.len() != 2 {
        eprintln!("basalt: --crash-test-workspace-undo requires a workspace path and change ID");
        std::process::exit(2);
    }
    run_workspace(&["undo".to_string(), args[0].clone(), args[1].clone()]);
}

fn open_database(path: &str) -> Result<Database, basalt::db::DbError> {
    if path == ":memory:" {
        Ok(Database::in_memory())
    } else {
        Database::open(path)
    }
}

/// Private child-process hook used by the crash-recovery integration test.
/// It intentionally leaves committed frames in the WAL and waits for the
/// parent to terminate it abruptly.
fn crash_test_writer(path: Option<&str>) {
    let Some(path) = path else {
        eprintln!("basalt: --crash-test-writer requires a database path");
        std::process::exit(2);
    };
    let database = match Database::open(path) {
        Ok(database) => database,
        Err(error) => {
            eprintln!("basalt: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = database.execute_sql(
        "CREATE TABLE crash_probe (id INTEGER PRIMARY KEY, value TEXT); INSERT INTO crash_probe VALUES (1, 'durable');",
    ) {
        eprintln!("basalt: {error}");
        std::process::exit(1);
    }
    println!("ready");
    let _ = io::stdout().flush();
    loop {
        std::thread::park_timeout(Duration::from_secs(60));
    }
}
