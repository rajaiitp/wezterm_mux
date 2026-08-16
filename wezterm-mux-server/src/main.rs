use clap::*;
use codec::ListPanesResponse;
use config::configuration;
use config::keyassignment::SpawnTabDomain;
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain, SplitSource};
use mux::tab::{PaneEntry, PaneNode, SplitDirection, SplitRequest, SplitSize, TabId};
use mux::window::WindowId;
use mux::Mux;
use mux::MuxNotification;
use portable_pty::cmdbuilder::CommandBuilder;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};
use wezterm_gui_subcommands::*;
use wezterm_mux_server_impl::update_mux_domains_for_server;
use wezterm_session_state::{self as session_state, SessionSnapshot};

mod daemonize;

#[derive(Debug, Parser)]
#[command(
    about = "Wez's Terminal Emulator\nhttp://github.com/wezterm/wezterm",
    version = config::wezterm_version(),
    trailing_var_arg = true,
)]
struct Opt {
    /// Skip loading wezterm.lua
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use, overrides the normal
    /// configuration file resolution
    #[arg(
        long,
        value_parser,
        conflicts_with = "skip_config",
        value_hint=ValueHint::FilePath,
    )]
    config_file: Option<OsString>,

    /// Override specific configuration values
    #[arg(
        long = "config",
        name = "name=value",
        value_parser=clap::builder::ValueParser::new(name_equals_value),
        number_of_values = 1)]
    config_override: Vec<(String, String)>,

    /// Detach from the foreground and become a background process
    #[arg(long = "daemonize")]
    daemonize: bool,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long = "cwd", value_parser, value_hint=ValueHint::DirPath)]
    cwd: Option<OsString>,

    #[cfg(unix)]
    #[arg(long, hide = true)]
    pid_file_fd: Option<i32>,

    /// Instead of executing your shell, run PROG.
    /// For example: `wezterm start -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

fn main() {
    if let Err(err) = run() {
        wezterm_blob_leases::clear_storage();
        log::error!("{:#}", err);
        std::process::exit(1);
    }
    wezterm_blob_leases::clear_storage();
}

fn run() -> anyhow::Result<()> {
    env_bootstrap::bootstrap();

    //stats::Stats::init()?;
    config::designate_this_as_the_main_thread();
    let _saver = umask::UmaskSaver::new();

    let opts = Opt::parse();

    #[cfg(unix)]
    {
        // Ensure that we set CLOEXEC on the inherited lock file
        // before we have an opportunity to spawn any child processes.
        if let Some(fd) = opts.pid_file_fd {
            daemonize::set_cloexec(fd, true);
        }
    }

    config::common_init(
        opts.config_file.as_ref(),
        &opts.config_override,
        opts.skip_config,
    )?;

    let config = config::configuration();

    config.update_ulimit()?;
    if let Some(value) = &config.default_ssh_auth_sock {
        std::env::set_var("SSH_AUTH_SOCK", value);
    }

    #[cfg(unix)]
    let mut pid_file = None;

    #[cfg(unix)]
    {
        if opts.daemonize {
            pid_file = daemonize::daemonize(&config)?;
            // When we reach this line, we are in a forked child process,
            // and the fork will have broken the async-io/reactor state
            // of the smol runtime.
            // To resolve this, we will re-exec ourselves in the block
            // below that was originally Windows-specific
        }
    }

    if opts.daemonize {
        // On Windows we can't literally daemonize, but we can spawn another copy
        // of ourselves in the background!
        // On Unix, forking breaks the global state maintained by `smol`,
        // so we need to re-exec ourselves to start things back up properly.
        let mut cmd = Command::new(std::env::current_exe().unwrap());

        #[cfg(unix)]
        {
            // Inform the new version of ourselves that we already
            // locked the pidfile so that it can prevent it from
            // being propagated to its children when they spawn
            if let Some(fd) = pid_file {
                cmd.arg("--pid-file-fd");
                cmd.arg(&fd.to_string());
            }
        }
        if opts.skip_config {
            cmd.arg("-n");
        }
        if let Some(f) = &opts.config_file {
            cmd.arg("--config-file");
            cmd.arg(f);
        }
        for (name, value) in &opts.config_override {
            cmd.arg("--config");
            cmd.arg(&format!("{name}={value}"));
        }
        if let Some(cwd) = opts.cwd {
            cmd.arg("--cwd");
            cmd.arg(cwd);
        }
        if !opts.prog.is_empty() {
            cmd.arg("--");
            for a in &opts.prog {
                cmd.arg(a);
            }
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.stdout(config.daemon_options.open_stdout()?);
            cmd.stderr(config.daemon_options.open_stderr()?);

            cmd.creation_flags(winapi::um::winbase::DETACHED_PROCESS);
            let child = cmd.spawn();
            drop(child);
            return Ok(());
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if let Some(mask) = umask::UmaskSaver::saved_umask() {
                unsafe {
                    cmd.pre_exec(move || {
                        libc::umask(mask);
                        Ok(())
                    });
                }
            }

            return Err(anyhow::anyhow!("failed to re-exec: {:?}", cmd.exec()));
        }
    }

    // Remove some environment variables that aren't super helpful or
    // that are potentially misleading when we're starting up the
    // server.
    // We may potentially want to look into starting/registering
    // a session of some kind here as well in the future.
    for name in &[
        "OLDPWD",
        "PWD",
        "SHLVL",
        "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET",
        "_",
    ] {
        std::env::remove_var(name);
    }
    for name in &config::configuration().mux_env_remove {
        std::env::remove_var(name);
    }

    wezterm_blob_leases::register_storage(Arc::new(
        wezterm_blob_leases::simple_tempdir::SimpleTempDir::new_in(&*config::CACHE_DIR)?,
    ))?;

    let need_builder = !opts.prog.is_empty() || opts.cwd.is_some();

    let cmd = if need_builder {
        let mut builder = if opts.prog.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            CommandBuilder::from_argv(opts.prog)
        };
        if let Some(cwd) = opts.cwd {
            builder.cwd(cwd);
        }
        Some(builder)
    } else {
        None
    };

    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
    let mux = Arc::new(mux::Mux::new(Some(domain.clone())));
    Mux::set_mux(&mux);

    let executor = promise::spawn::SimpleExecutor::new();

    spawn_listener().map_err(|e| {
        log::error!("problem spawning listeners: {:?}", e);
        e
    })?;

    let activity = Activity::new();

    promise::spawn::spawn(async move {
        if let Err(err) = async_run(cmd).await {
            terminate_with_error(err);
        }
        drop(activity);
    })
    .detach();

    loop {
        executor.tick()?;
    }
}

async fn trigger_mux_startup(lua: Option<Rc<mlua::Lua>>) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi(())?;
        config::lua::emit_event(&lua, ("mux-startup".to_string(), args)).await?;
    }
    Ok(())
}

fn native_snapshot_path() -> Option<PathBuf> {
    wezterm_session_state::default_path().ok()
}

fn native_session_enabled(name: &str, default: bool) -> bool {
    match env::var(name).ok().as_deref() {
        Some("0") | Some("false") | Some("no") => false,
        Some("1") | Some("true") | Some("yes") => true,
        _ => default,
    }
}

fn capture_mux() -> ListPanesResponse {
    let mux = Mux::get();
    let mut tabs = vec![];
    let mut tab_titles = vec![];
    let mut window_titles = std::collections::HashMap::new();
    let mut active_tabs = std::collections::HashMap::new();
    for window_id in mux.iter_windows() {
        if let Some(window) = mux.get_window(window_id) {
            if let Some(tab) = window.get_active() {
                active_tabs.insert(window_id, tab.tab_id());
            }
            window_titles.insert(window_id, window.get_title().to_string());
            for tab in window.iter() {
                tabs.push(tab.codec_pane_tree());
                tab_titles.push(tab.get_title());
            }
        }
    }
    ListPanesResponse {
        tabs,
        tab_titles,
        window_titles,
        active_tabs,
    }
}

fn save_native_snapshot() -> anyhow::Result<bool> {
    if !native_session_enabled("WEZTERM_HERDR_NATIVE_SESSION_AUTOSAVE", true) {
        return Ok(false);
    }
    let mux = capture_mux();
    if mux.tabs.is_empty() {
        return Ok(false);
    }
    let path = session_state::default_path()?;
    session_state::write_atomic(&path, &SessionSnapshot::new(mux))?;
    Ok(true)
}

fn start_native_autosave() {
    if !native_session_enabled("WEZTERM_HERDR_NATIVE_SESSION_AUTOSAVE", true) {
        return;
    }
    let interval = env::var("WEZTERM_HERDR_NATIVE_SESSION_INTERVAL")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        // Keep a recent rolling snapshot so a crash or forced restart can
        // recover project applications even when the user did not run the
        // explicit restart helper.
        .unwrap_or(15)
        .max(5);
    let dirty = Arc::new(AtomicBool::new(true));
    let periodic_dirty = Arc::clone(&dirty);
    Mux::get().subscribe_persistence(move |notification| {
        if matches!(
            notification,
            MuxNotification::PaneAdded(_)
                | MuxNotification::PaneRemoved(_)
                | MuxNotification::WindowCreated(_)
                | MuxNotification::WindowRemoved(_)
                | MuxNotification::TabAddedToWindow { .. }
                | MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::WorkspaceRenamed { .. }
                | MuxNotification::TabTitleChanged { .. }
                | MuxNotification::WindowTitleChanged { .. }
                | MuxNotification::PaneFocused(_)
                | MuxNotification::TabResized(_)
        ) {
            dirty.store(true, Ordering::Release);
        }
        true
    });
    promise::spawn::spawn(async move {
        loop {
            smol::Timer::after(Duration::from_secs(interval)).await;
            if periodic_dirty.swap(false, Ordering::AcqRel) {
                if let Err(err) = smol::unblock(save_native_snapshot).await {
                    log::warn!("native session periodic save failed: {err:#}");
                    periodic_dirty.store(true, Ordering::Release);
                }
            }
        }
    })
    .detach();
}

#[derive(Debug, Clone)]
struct RestoredPane {
    saved: PaneEntry,
    actual: mux::pane::PaneId,
}

fn collect_leaves(node: PaneNode, leaves: &mut Vec<PaneEntry>) {
    match node {
        PaneNode::Empty => {}
        PaneNode::Leaf(entry) => leaves.push(entry),
        PaneNode::Split { left, right, .. } => {
            collect_leaves(*left, leaves);
            collect_leaves(*right, leaves);
        }
    }
}

fn command_dir(entry: &PaneEntry) -> Option<String> {
    entry
        .working_dir
        .as_ref()
        .and_then(|url| url.url.to_file_path().ok())
        .and_then(|path| path.to_str().map(ToOwned::to_owned))
}

fn shell_command(config: &config::ConfigHandle) -> Option<Vec<OsString>> {
    config
        .default_prog
        .clone()
        .map(|prog| prog.into_iter().map(OsString::from).collect())
}

fn restore_command(entry: &PaneEntry, _config: &config::ConfigHandle) -> Option<CommandBuilder> {
    let process = project_app_process(entry.process.as_ref()?);
    let command = if is_app(&process, &["nvim", "vim", "vi", "neovim"]) {
        editor_restore_command(&process)
    } else if is_app(&process, &["pi"]) {
        pi_restore_command(entry, &process)
    } else if is_app(&process, &["claude", "codex", "tuxedo", "tuicr"]) {
        recorded_restore_command(&process)
    } else {
        return None;
    };
    Some(CommandBuilder::from_argv(shell_backed_argv(command)))
}

fn project_app_process(process: &mux::tab::PaneProcessInfo) -> mux::tab::PaneProcessInfo {
    let Some(marker) = process
        .argv
        .iter()
        .position(|arg| arg == "wezterm-project-app")
    else {
        return process.clone();
    };
    let argv = process.argv.iter().skip(marker + 1).cloned().collect::<Vec<_>>();
    let Some(executable) = argv.first() else {
        return process.clone();
    };
    let name = Path::new(executable)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(executable)
        .to_string();
    mux::tab::PaneProcessInfo {
        name,
        executable: executable.clone(),
        argv,
        cwd: process.cwd.clone(),
    }
}

fn shell_backed_argv(command: Vec<OsString>) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("/bin/sh"),
        OsString::from("-lc"),
        OsString::from(r#""$@"; exec "${SHELL:-/bin/sh}" -l"#),
        OsString::from("wezterm-project-app"),
    ];
    argv.extend(command);
    argv
}

fn recorded_restore_command(process: &mux::tab::PaneProcessInfo) -> Vec<OsString> {
    if !process.argv.is_empty() {
        return process.argv.iter().map(OsString::from).collect();
    }
    let executable = if process.executable.is_empty() {
        if process.name.is_empty() { "sh" } else { process.name.as_str() }
    } else {
        &process.executable
    };
    vec![OsString::from(executable)]
}

fn editor_restore_command(process: &mux::tab::PaneProcessInfo) -> Vec<OsString> {
    let executable = if process.executable.is_empty() {
        if process.name.is_empty() { OsString::from("nvim") } else { OsString::from(&process.name) }
    } else {
        OsString::from(&process.executable)
    };
    let mut args: Vec<OsString> = process
        .argv
        .iter()
        .filter(|arg| arg.as_str() != "--embed")
        .map(OsString::from)
        .collect();
    let argv_has_editor = args
        .first()
        .and_then(|arg| Path::new(arg).file_name())
        .and_then(|name| name.to_str())
        .map(|name| ["nvim", "vim", "vi", "neovim"].iter().any(|editor| {
            name == *editor || name.starts_with(&format!("{editor}."))
        }))
        .unwrap_or(false);
    if !argv_has_editor {
        args.insert(0, executable);
    }
    if args.is_empty() {
        args.push(OsString::from("nvim"));
    }
    args
}

fn is_app(process: &mux::tab::PaneProcessInfo, names: &[&str]) -> bool {
    let mut values = Vec::with_capacity(process.argv.len() + 2);
    values.push(process.name.as_str());
    values.push(process.executable.as_str());
    values.extend(process.argv.iter().map(String::as_str));
    values.iter().any(|value| {
        let value = value.to_ascii_lowercase();
        let basename = Path::new(&value)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(value.as_str());
        names.iter().any(|name| {
            basename == *name
                || basename.starts_with(&format!("{name}."))
                || basename == format!("{name}.exe")
        })
    })
}

fn pi_restore_command(entry: &PaneEntry, process: &mux::tab::PaneProcessInfo) -> Vec<OsString> {
    if process
        .argv
        .iter()
        .any(|arg| arg == "--session" || arg == "--no-session" || arg == "--fork")
    {
        return process.argv.iter().map(OsString::from).collect();
    }
    if let Some(path) = latest_pi_session(entry, process) {
        return vec![OsString::from("pi"), OsString::from("--session"), path.into_os_string()];
    }
    vec![OsString::from("pi"), OsString::from("-c")]
}

fn latest_pi_session(entry: &PaneEntry, process: &mux::tab::PaneProcessInfo) -> Option<PathBuf> {
    let cwd = if process.cwd.is_empty() {
        entry.working_dir.as_ref()?.url.to_file_path().ok()?
    } else {
        PathBuf::from(&process.cwd)
    };
    let home = dirs_next::home_dir()?;
    let encoded = cwd.to_string_lossy().trim_start_matches('/').replace('/', "-");
    let session_dir = home
        .join(".pi")
        .join("agent")
        .join("sessions")
        .join(format!("--{encoded}--"));
    fs::read_dir(session_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max_by_key(|(modified, _)| modified.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default())
        .map(|(_, path)| path)
}

fn split_for(entry: &PaneEntry, restored: &[RestoredPane]) -> (mux::pane::PaneId, SplitRequest) {
    let target = restored
        .iter()
        .min_by_key(|candidate| {
            candidate.saved.left_col.abs_diff(entry.left_col)
                + candidate.saved.top_row.abs_diff(entry.top_row)
        })
        .expect("at least one restored pane");
    let horizontal = entry.left_col >= target.saved.left_col + target.saved.size.cols;
    let vertical = entry.top_row >= target.saved.top_row + target.saved.size.rows;
    let direction = if horizontal {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    };
    let size = if horizontal {
        entry.size.cols
    } else {
        entry.size.rows
    };
    (
        target.actual,
        SplitRequest {
            direction,
            target_is_second: horizontal || vertical,
            top_level: false,
            size: SplitSize::Cells(size.max(1)),
        },
    )
}

async fn restore_snapshot_into_mux(
    snapshot: wezterm_session_state::SessionSnapshot,
    config: &config::ConfigHandle,
) -> anyhow::Result<()> {
    let mux = Mux::get();
    let mut windows = HashMap::<WindowId, WindowId>::new();
    let mut active_panes = HashMap::<TabId, mux::pane::PaneId>::new();
    for (root, title) in snapshot
        .mux
        .tabs
        .into_iter()
        .zip(snapshot.mux.tab_titles.into_iter())
    {
        let Some((old_window, old_tab)) = root.window_and_tab_ids() else {
            continue;
        };
        let Some(size) = root.root_size() else {
            continue;
        };
        let mut leaves = vec![];
        collect_leaves(root, &mut leaves);
        leaves.sort_by_key(|entry| (entry.top_row, entry.left_col));
        let first = leaves
            .first()
            .ok_or_else(|| anyhow::anyhow!("snapshot tab has no panes"))?;
        let (tab, pane, window) = mux
            .spawn_tab_or_window(
                windows.get(&old_window).copied(),
                SpawnTabDomain::DefaultDomain,
                restore_command(first, config)
                    .or_else(|| shell_command(config).map(CommandBuilder::from_argv)),
                command_dir(first),
                size,
                None,
                first.workspace.clone(),
                None,
            )
            .await?;
        windows.insert(old_window, window);
        tab.set_title(&title);
        if let Some(window_title) = snapshot.mux.window_titles.get(&old_window) {
            if let Some(mut window_ref) = mux.get_window_mut(window) {
                window_ref.set_title(window_title);
            }
        }
        let mut restored = vec![RestoredPane {
            saved: first.clone(),
            actual: pane.pane_id(),
        }];
        if first.is_active_pane {
            active_panes.insert(old_tab, pane.pane_id());
        }
        for entry in leaves.into_iter().skip(1) {
            let (target, request) = split_for(&entry, &restored);
            let (new_pane, _) = mux
                .split_pane(
                    target,
                    request,
                    SplitSource::Spawn {
                        command: restore_command(&entry, config)
                            .or_else(|| shell_command(config).map(CommandBuilder::from_argv)),
                        command_dir: command_dir(&entry),
                    },
                    SpawnTabDomain::CurrentPaneDomain,
                )
                .await?;
            if entry.is_active_pane {
                active_panes.insert(old_tab, new_pane.pane_id());
            }
            restored.push(RestoredPane {
                saved: entry,
                actual: new_pane.pane_id(),
            });
        }
    }
    for (old_window, old_tab) in snapshot.mux.active_tabs {
        if let (Some(window), Some(pane_id)) =
            (windows.get(&old_window), active_panes.get(&old_tab))
        {
            mux.focus_pane_and_containing_tab(*pane_id)?;
            let _ = window;
        }
    }
    Ok(())
}

async fn restore_native_snapshot(config: &config::ConfigHandle) -> anyhow::Result<bool> {
    if !native_session_enabled("WEZTERM_HERDR_NATIVE_SESSION_RESTORE", true) {
        return Ok(false);
    }
    let Some(path) = native_snapshot_path() else {
        return Ok(false);
    };
    if !path.exists() {
        return Ok(false);
    }
    let snapshot = session_state::read(&path)?;
    restore_snapshot_into_mux(snapshot, config).await?;
    if let Err(err) = fs::remove_file(&path) {
        log::warn!(
            "restored native snapshot but could not consume {}: {err}",
            path.display()
        );
    }
    log::info!("restored native session snapshot directly into persistent mux");
    Ok(true)
}

async fn async_run(cmd: Option<CommandBuilder>) -> anyhow::Result<()> {
    let mux = Mux::get();
    let config = config::configuration();

    update_mux_domains_for_server(&config)?;
    let domain = mux.default_domain();
    let _config_subscription = config::subscribe_to_config_reload(move || {
        promise::spawn::spawn_into_main_thread(async move {
            if let Err(err) = update_mux_domains_for_server(&config::configuration()) {
                log::error!("Error updating mux domains: {:#}", err);
            }
        })
        .detach();
        true
    });

    {
        if let Err(err) = config::with_lua_config_on_main_thread(trigger_mux_startup).await {
            log::error!("while processing mux-startup event: {:#}", err);
        }
    }

    // A snapshot is only a fallback for an empty mux. Checking all windows
    // avoids duplicating sessions when a non-default domain already owns a
    // live window.
    if mux.iter_windows().is_empty() {
        let restored = match restore_native_snapshot(&config).await {
            Ok(restored) => restored,
            Err(err) => {
                log::warn!("native snapshot restore failed: {err:#}");
                false
            }
        };

        if !restored {
            let workspace = None;
            let position = None;
            let window_id = mux.new_empty_window(workspace, position);
            domain.attach(Some(*window_id)).await?;

            let _tab = mux
                .default_domain()
                .spawn(config.initial_size(0, None), cmd, None, *window_id)
                .await?;
        }
    }

    // Persistence belongs to the long-lived mux process. GUI clients do not
    // independently restore or periodically overwrite this snapshot.
    start_native_autosave();
    Ok(())
}

fn terminate_with_error(err: anyhow::Error) -> ! {
    log::error!("{:#}; terminating", err);
    std::process::exit(1);
}

mod ossl;

pub fn spawn_listener() -> anyhow::Result<()> {
    let config = configuration();
    for unix_dom in &config.unix_domains {
        std::env::set_var(
            format!("WEZTERM_UNIX_SOCKET_{}", unix_dom.name.to_ascii_uppercase()),
            unix_dom.socket_path(),
        );
        std::env::set_var(
            format!(
                "WEZTERM_AUTOMATION_SOCKET_{}",
                unix_dom.name.to_ascii_uppercase()
            ),
            wezterm_mux_server_impl::automation::socket_path(unix_dom),
        );
        std::env::set_var("WEZTERM_UNIX_SOCKET", unix_dom.socket_path());
        let mut listener = wezterm_mux_server_impl::local::LocalListener::with_domain(unix_dom)?;
        thread::spawn(move || {
            listener.run();
        });

        let automation = wezterm_mux_server_impl::automation::spawn_listener(unix_dom)?;
        // Keep the legacy unqualified value for the first/default domain while
        // also exposing every configured local domain explicitly.
        if std::env::var_os("WEZTERM_AUTOMATION_SOCKET").is_none() {
            std::env::set_var("WEZTERM_AUTOMATION_SOCKET", automation);
        }
    }

    for tls_server in &config.tls_servers {
        ossl::spawn_tls_listener(tls_server)?;
    }

    Ok(())
}
