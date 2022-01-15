use argh::FromArgs;
use crossterm::{
    event,
    event::KeyCode,
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use directories;
use jex::{
    app::{App, AppRenderMode, Focus},
    cursor::GlobalCursor,
    helper::Helper,
    layout::JexLayout,
    view_tree::View,
};
use log::{debug, warn};
use regex::Regex;
use reqwest::Url;
use simplelog::WriteLogger;
use std::{
    default::Default,
    error::Error,
    fs,
    fs::{create_dir_all, File},
    io,
    io::Write,
    panic,
    path::PathBuf,
};
use tui::{
    backend::CrosstermBackend,
    layout::Rect,
    widgets::{Block, Borders},
    Frame, Terminal,
};
use unicode_width::UnicodeWidthStr;

#[cfg(feature = "dev-tools")]
use cpuprofiler::PROFILER;
#[cfg(feature = "dev-tools")]
use prettytable::{cell, ptable, row, table, Table};

#[derive(FromArgs, PartialEq, Debug)]
/// Json viewer and editor
struct Args {
    #[cfg(feature = "dev-tools")]
    #[argh(subcommand)]
    mode: Mode,
    #[argh(option)]
    #[argh(description = "logging level")]
    #[argh(default = "log::LevelFilter::Warn")]
    log_level: log::LevelFilter,
    #[argh(option)]
    #[argh(description = "logging output file")]
    log_path: Option<String>,
    #[argh(positional)]
    json_path: String,
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand)]
enum Mode {
    Normal(NormalMode),
    Bench(BenchMode),
}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "load")]
/// Run the editor
struct NormalMode {}

#[derive(FromArgs, PartialEq, Debug)]
#[argh(subcommand, name = "bench")]
/// Benchmark loading a json file
struct BenchMode {}

// Large file perf (181 mb):
// * Old: 13.68 sec
//   * Initial parsing (serde): 3.77 sec
//   * Pre-rendering (lines): 2.29 sec (left and right)
//   * Query execution: 7.62 sec
//     * Serde -> JV: 3.38 sec
//     * Computing result: 0???? (it is the trivial filter)
//     * JV -> Serde: 3.37 sec
// * New: 6.32 sec
//   * Initial parsing (JV deserialize): 6.26
//   * Query execution: ~0
//
// What can we do to improve load times? The current situation looks bleak.
// * If (big if) JV iterated through maps in insertion order, you could imagine rendinering the
// scene before the file is fully loaded. We can't load instantly, but we can definitely load one
// page of json instantly. Probably worth reading the JV object implementation: hopefully it's not
// too complicated.
// * We might be able to deserialize in parallel.
// * Use private JV functions to bypass typechecking when we already know the type.
// * Only use JVRaws duing deserialization.
// * Stop using JQ entirely (this would be hellish)
// * If you can guarantee identiacal rendering from JV and serde Values, deserialize into a serde
// Value (faster), become interactive then, and secretly swap in the JV once that's ready. Not
// great from a memory perspective. Any way to do that incrementally? Since we'd have full control
// over the value-like structure, it might be doable. Shared mutable access across different
// threads is.... a concenrn.
// * Completely violate the JV privacy boundary and construct JVs directly. Would we be able to
// make it faster? I'd be surprised: my guess is that the JV implementation is fairly optimal
// _given_ the datastructure, which we wouldn't be able to avoid.
// * Write an interpreter for JQ bytecode. That's definitely considered an implementation detail,
// so that would be pretty evil, but we might be able to operate directly on serde Values.
//
// TODO
// * Edit tree:
//   * Children can be modified if they have no children
//   * Allow copying descendents onto another root, so you if you want to modify a tree's root you
// can do so by making a new root and then copying over the descendents
// * Lightweight error messages (no search results, can't fold a leaf, can't edit a non-leaf)
//   * Probably requires timers, which requires us to be able to inject stuff into the event
//   stream. Async? That would also let us show a loading message.
// * Diffs
//   * UI
//     * Need to make left and right pane independent
//     * Query is with respect to parent, which may not be visible
//     * Root nodes have no query
//     * Once this is implemented, can turn on diffing
//   * Backend
//     * Current implementation kind of sucks since it needs O(n) memory
//     * Meyer diff may require O(n) memory anyway, but no need to do it twice
//     * Index trait is a problem for anything fancy since you need to return a reference
//     * Meyer will return results in usize ranges. Need to be able to interpret those
//     * Cursor needs to track the diff-element index to make this reverse mapping possible
//   * Plan
//     * Do the UI stuff
//     * Add index tracking to the cursor
//     * Get an MVP with the stupid allocating thing working
// * Rip out rustyline, or just use it's guts?
//
//
// Rendering pipeline:
// * Vec<JV>
// * LeafCursor
// * Leaf
// * LineFragments
// * LineCursor
// * UnstyledSpans
// * Spans

#[cfg(feature = "dev-tools")]
fn main() -> Result<(), Box<dyn Error>> {
    use coredump;
    coredump::register_panic_handler();
    let args: Args = argh::from_env();
    init_logging(&args);
    match args.mode {
        Mode::Normal(_) => run(args.json_path),
        Mode::Bench(_) => bench(args.json_path),
    }
}

#[cfg(not(feature = "dev-tools"))]
fn main() -> Result<(), Box<dyn Error>> {
    let args: Args = argh::from_env();
    init_logging(&args);
    run(args.json_path)
}

fn init_logging(args: &Args) {
    if let Some(path) = args.log_path.as_ref() {
        let fout = File::create(path).expect("Couldn't create log file");
        WriteLogger::init(args.log_level, Default::default(), fout)
            .expect("Couldn't initalize logger");
    }
}

fn force_draw<B: tui::backend::Backend, F: FnMut(&mut Frame<B>)>(
    terminal: &mut Terminal<B>,
    mut f: F,
) -> Result<(), io::Error> {
    terminal.autoresize()?;
    let mut frame = terminal.get_frame();
    f(&mut frame);
    let current_buffer = terminal.current_buffer_mut().clone();
    terminal.current_buffer_mut().reset();
    terminal.draw(f)?;
    let area = current_buffer.area;
    let width = area.width;

    let mut updates: Vec<(u16, u16, &tui::buffer::Cell)> = vec![];
    // Cells from the current buffer to skip due to preceeding multi-width characters taking their
    // place (the skipped cells should be blank anyway):
    let mut to_skip: usize = 0;
    for (i, current) in current_buffer.content.iter().enumerate() {
        if to_skip == 0 {
            let x = i as u16 % width;
            let y = i as u16 / width;
            updates.push((x, y, &current_buffer.content[i]));
        }

        to_skip = current.symbol.width().saturating_sub(1);
    }
    terminal.backend_mut().draw(updates.into_iter())
}

struct DeferRestoreTerminal {}

impl Drop for DeferRestoreTerminal {
    fn drop(&mut self) {
        disable_raw_mode().expect("Failed to disable raw mode");
        execute!(io::stdout(), LeaveAlternateScreen).expect("Failed to leave alternate screen");
    }
}

struct RustylineWrapper {
    history_path: PathBuf,
    editor: rustyline::Editor<Helper>,
}

impl RustylineWrapper {
    fn new(history_path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let config = rustyline::Config::builder().auto_add_history(true).build();
        let mut editor = rustyline::Editor::with_config(config);
        let _ = editor.history_mut().load(&history_path);
        editor.bind_sequence(rustyline::KeyPress::Esc, rustyline::Cmd::Interrupt);
        Ok(RustylineWrapper {
            history_path,
            editor,
        })
    }
}
impl Drop for RustylineWrapper {
    fn drop(&mut self) {
        let res = create_dir_all(self.history_path.parent().unwrap());
        if let Err(err) = res {
            warn!("Error creating directory: {:?}", err);
        }
        let res = self.editor.history().save(&self.history_path);
        if let Err(err) = res {
            warn!("Error saving history: {:?}", err);
        }
    }
}

fn run(json_path: String) -> Result<(), Box<dyn Error>> {
    enable_raw_mode().expect("Failed to enter raw mode");

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).expect("Failed to enter alternate screen");
    let default_panic_handler = panic::take_hook();
    panic::set_hook(Box::new(move |p| {
        disable_raw_mode().expect("Failed to disable raw mode");
        execute!(io::stdout(), LeaveAlternateScreen).expect("Failed to leave alternate screen");
        default_panic_handler(p);
    }));
    let _defer = DeferRestoreTerminal {};
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let initial_layout = JexLayout::new(terminal.get_frame().size(), false);

    // NOTE: see also open_file, can these be refactored to one?
    let mut app = if let Ok(url) = Url::parse(&json_path.as_str()) {
        let body = reqwest::blocking::get(url.as_str())?;
        let app = App::new(body, json_path, initial_layout)?;
        app
    } else {
        let f = fs::File::open(&json_path)?;
        let buf = io::BufReader::new(f);
        let app = App::new(buf, json_path, initial_layout)?;
        app
    };

    terminal.draw(app.render(AppRenderMode::Normal))?;
    let project_dirs =
        directories::ProjectDirs::from("", "", "jex").ok_or("Error getting project dirs")?;
    let cache_dir = project_dirs.cache_dir();
    let mut query_rl = RustylineWrapper::new(cache_dir.join("query_history"))?;
    let mut search_rl = RustylineWrapper::new(cache_dir.join("search_history"))?;
    let mut open_rl = RustylineWrapper::new(cache_dir.join("open_history"))?;
    let mut rename_rl = RustylineWrapper::new(cache_dir.join("rename_history"))?;
    let mut save_rl = RustylineWrapper::new(cache_dir.join("save_history"))?;

    open_rl.editor.set_helper(Some(Helper::new()));
    save_rl.editor.set_helper(Some(Helper::new()));
    loop {
        let event = event::read().expect("Error getting next event");
        debug!("Event: {:?}", event);
        let c = match event {
            event::Event::Key(c) => c,
            event::Event::Mouse(_) => panic!("Mouse events aren't enabled!"),
            event::Event::Resize(width, height) => {
                let rect = Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                let layout = JexLayout::new(rect, app.show_tree);
                app.resize(layout);
                terminal.draw(app.render(AppRenderMode::Normal))?;
                continue;
            }
        };
        let layout = JexLayout::new(terminal.get_frame().size(), app.show_tree);
        if let Some(flash) = app.flash.as_mut() {
            match c.code {
                KeyCode::Esc => {
                    app.flash = None;
                }
                KeyCode::Down => {
                    flash.scroll = flash.scroll.saturating_add(1);
                }
                KeyCode::Up => {
                    flash.scroll = flash.scroll.saturating_sub(1);
                }
                _ => {}
            }
            terminal.draw(app.render(AppRenderMode::Normal))?;
            continue;
        }
        match c.code {
            KeyCode::Esc => break,
            KeyCode::Char('t') => {
                app.show_tree = !app.show_tree;
            }
            KeyCode::Char('q') => {
                if app.focused_query_mut().is_some() {
                    terminal.draw(app.render(AppRenderMode::InputEditor))?;
                    let query = app.focused_query_mut().unwrap();
                    match query_rl.editor.readline_with_initial("", (&*query, "")) {
                        Ok(new_query) => {
                            *query = new_query;
                            // Just in case rustyline messed stuff up
                            force_draw(&mut terminal, app.render(AppRenderMode::Normal))?;
                            app.recompute_focused_view(layout.right);
                        }
                        Err(_) => {}
                    }
                }
            }
            KeyCode::Tab => {
                app.focus = app.focus.swap();
                debug!("Swapped focus to {:?}", app.focus);
            }
            KeyCode::Char('+') => {
                let (index, rect) = match app.focus {
                    Focus::Left => (&app.left_index, layout.left),
                    Focus::Right => (&app.right_index, layout.right),
                };
                let tree = app.views.trees[index.tree]
                    .index_tree_mut(&index.within_tree.path)
                    .expect("App index invalidated");
                tree.push_trivial_child(rect);
            }
            KeyCode::Char('j') => match app.focus {
                Focus::Left => {
                    app.left_index.advance(&app.views);
                }
                Focus::Right => {
                    app.right_index.advance(&app.views);
                }
            },
            KeyCode::Char('k') => match app.focus {
                Focus::Left => {
                    app.left_index.regress(&app.views);
                }
                Focus::Right => {
                    app.right_index.regress(&app.views);
                }
            },
            KeyCode::Char('r') => {
                terminal.draw(app.render(AppRenderMode::InputEditor))?;
                let mut view_with_parent = app.focused_view_mut();
                let frame = view_with_parent.frame();
                match rename_rl
                    .editor
                    .readline_with_initial("New Title:", (&frame.name, ""))
                {
                    Ok(new_name) => {
                        frame.name = new_name;
                    }
                    Err(_) => {}
                }
                force_draw(&mut terminal, app.render(AppRenderMode::Normal))?;
            }
            KeyCode::Char('s') => {
                terminal.draw(app.render(AppRenderMode::InputEditor))?;
                let mut view_with_parent = app.focused_view_mut();
                let frame = view_with_parent.frame();
                let flash = {
                    if let View::Json(Some(view)) = &frame.view {
                        match save_rl
                            .editor
                            .readline_with_initial("Save to:", (&frame.name, ""))
                        {
                            Ok(path) => {
                                if let Err(err) = view.save_to(&path) {
                                    Some(format!("Error saving json:\n{:?}", err))
                                } else {
                                    frame.name = path;
                                    let focused_index = app.focused_index().clone();
                                    app.re_root(&focused_index);
                                    None
                                }
                            }
                            Err(_) => None,
                        }
                    } else {
                        None
                    }
                };
                if let Some(flash) = flash {
                    app.set_flash(flash);
                }
                force_draw(&mut terminal, app.render(AppRenderMode::Normal))?;
            }
            KeyCode::Char('o') => {
                terminal.draw(app.render(AppRenderMode::InputEditor))?;
                let flash = {
                    match open_rl.editor.readline("Open: ") {
                        Ok(path) => app.open_file(path, layout).err().map(|err| err.to_string()),
                        Err(_) => None,
                    }
                };
                if let Some(flash) = flash {
                    app.set_flash(flash);
                }
                force_draw(&mut terminal, app.render(AppRenderMode::Normal))?;
            }
            KeyCode::Char('h') | KeyCode::Char('?') | KeyCode::F(1) => {
                app.show_help();
            }
            _ => {}
        }
        let view_rect = match app.focus {
            Focus::Left => layout.left,
            Focus::Right => layout.right,
        };
        let mut view_with_parent = app.focused_view_mut();
        let view_frame = view_with_parent.frame();
        let json_rect = Block::default().borders(Borders::ALL).inner(view_rect);
        match &mut view_frame.view {
            View::Error(_) => {}
            View::Json(None) => {}
            View::Json(Some(view)) => {
                view.resize_to(json_rect);
                match c.code {
                    KeyCode::Down => {
                        view.advance_cursor();
                    }
                    KeyCode::Up => {
                        view.regress_cursor();
                    }
                    KeyCode::PageDown => {
                        view.page_down();
                    }
                    KeyCode::PageUp => {
                        view.page_up();
                    }
                    KeyCode::Char('z') => {
                        view.toggle_fold();
                    }
                    KeyCode::Char('/') => {
                        terminal.draw(app.render(AppRenderMode::InputEditor))?;
                        match search_rl.editor.readline_with_initial("Search:", ("", "")) {
                            Ok(new_search) => {
                                // Just in case rustyline messed stuff up
                                force_draw(&mut terminal, app.render(AppRenderMode::Normal))?;
                                app.search_re = Regex::new(new_search.as_ref()).ok();
                                app.search(false);
                            }
                            Err(_) => {}
                        }
                    }
                    KeyCode::Char('n') => {
                        app.search(false);
                    }
                    KeyCode::Char('N') => {
                        app.search(true);
                    }
                    KeyCode::Home => {
                        view.scroll =
                            GlobalCursor::new(view.values.clone(), view.rect.width, &view.folds)
                                .expect("values should still exist");
                        view.cursor = view.scroll.value_cursor.clone();
                    }
                    KeyCode::End => {
                        view.scroll = GlobalCursor::new_end(
                            view.values.clone(),
                            view.rect.width,
                            &view.folds,
                        )
                        .expect("values should still exist");
                        view.cursor = view.scroll.value_cursor.clone();
                    }
                    _ => {}
                };
            }
        }
        terminal.draw(app.render(AppRenderMode::Normal))?;
    }
    // Gracefully freeing the JV values can take a significant amount of time and doesn't actually
    // benefit anything: the OS will clean up after us when we exit.
    std::mem::forget(app);
    Ok(())
}

#[cfg(feature = "dev-tools")]
fn bench(json_path: String) -> Result<(), io::Error> {
    let mut profiler = PROFILER.lock().unwrap();
    profiler.start("profile").unwrap();
    let f = fs::File::open(&json_path)?;
    let r = io::BufReader::new(f);
    let initial_layout = JexLayout {
        left: Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        },
        right: Rect {
            x: 100,
            y: 0,
            width: 100,
            height: 100,
        },
        query: Rect {
            x: 0,
            y: 100,
            width: 100,
            height: 1,
        },
        tree: None,
    };
    let mut app = App::new(r, json_path, initial_layout)?;
    std::mem::forget(app);
    profiler.stop().unwrap();
    Ok(())
}
