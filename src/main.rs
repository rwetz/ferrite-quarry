#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod search;
mod settings;

use ferrite_design::prelude::*;
use gpui::{
    App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, IntoElement, KeyBinding,
    PathPromptOptions, Render, Subscription, Window, div, px, size,
};
use search::{Catalog, Hit, Scope};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Quarry {
    settings: settings::Settings,
    input: Entity<TextInput>,
    catalog: Option<Arc<Catalog>>,
    hits: Arc<Vec<Hit>>,
    total: usize,
    selected: Option<usize>,
    preview: Arc<Vec<(usize, String)>>,
    preview_error: Option<String>,
    scope: Scope,
    regex: bool,
    indexing: bool,
    searching: bool,
    error: Option<String>,
    progress: Arc<AtomicUsize>,
    scan_cancel: Arc<AtomicBool>,
    search_cancel: Arc<AtomicBool>,
    scan_generation: u64,
    search_generation: u64,
    preview_generation: u64,
    palette: Entity<CommandPalette>,
    toaster: Entity<Toaster>,
    _input: Subscription,
    _appearance: Subscription,
}
impl Quarry {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(window, cx)
                .placeholder("Search your files...")
                .prompt(">")
        });
        let subscription = cx.subscribe(&input, |this, _, _: &InputEvent, cx| this.search(cx));
        let mut app = Self {
            settings: settings::Settings::load(),
            input,
            catalog: None,
            hits: Arc::new(Vec::new()),
            total: 0,
            selected: None,
            preview: Arc::new(Vec::new()),
            preview_error: None,
            scope: Scope::All,
            regex: false,
            indexing: false,
            searching: false,
            error: None,
            progress: Arc::new(AtomicUsize::new(0)),
            scan_cancel: Arc::new(AtomicBool::new(false)),
            search_cancel: Arc::new(AtomicBool::new(false)),
            scan_generation: 0,
            search_generation: 0,
            preview_generation: 0,
            palette: cx.new(|cx| CommandPalette::new(window, cx)),
            toaster: cx.new(|_| Toaster::new()),
            _input: subscription,
            _appearance: theme::follow_system(window),
        };
        let weak = cx.weak_entity();
        let command_action = |f: fn(&mut Self, &mut Context<Self>)| {
            let weak = weak.clone();
            move |_: &mut Window, cx: &mut App| {
                let _ = weak.update(cx, f);
            }
        };
        app.palette.update(cx, |palette, cx| {
            palette.set_commands(
                vec![
                    command("Add search folder")
                        .group("Folders")
                        .icon(Icon::Plus)
                        .on_run(command_action(Self::add_folder)),
                    command("Refresh index")
                        .group("Search")
                        .icon(Icon::Refresh)
                        .on_run(command_action(Self::scan)),
                    command("Cancel indexing")
                        .group("Search")
                        .icon(Icon::Stop)
                        .on_run(command_action(Self::cancel_scan)),
                    command("Open selected file")
                        .group("Results")
                        .icon(Icon::File)
                        .on_run(command_action(Self::open_file)),
                    command("Reveal selected file")
                        .group("Results")
                        .icon(Icon::Folder)
                        .on_run(command_action(Self::reveal)),
                    command("Next result")
                        .group("Results")
                        .shortcut("Ctrl+J")
                        .on_run(command_action(Self::next)),
                    command("Previous result")
                        .group("Results")
                        .shortcut("Ctrl+K")
                        .on_run(command_action(Self::previous)),
                    command("Copy selected path")
                        .group("Results")
                        .icon(Icon::Copy)
                        .on_run(command_action(Self::copy_path)),
                ],
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.indexing {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        if !app.settings.folders.is_empty() {
            app.scan(cx);
        }
        app
    }
    fn notice(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.toaster.update(cx, |toaster, cx| {
            toaster.push(toast(message.into()).danger(), cx)
        });
    }
    fn add_folder(&mut self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add search folders".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                for path in paths {
                    if !this.settings.folders.contains(&path) {
                        this.settings.folders.push(path);
                    }
                }
                if let Err(error) = this.settings.save() {
                    this.notice(error, cx);
                }
                this.scan(cx);
            });
        })
        .detach();
    }
    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        self.scan_cancel.store(true, Ordering::Relaxed);
        cx.notify();
    }
    fn scan(&mut self, cx: &mut Context<Self>) {
        self.scan_cancel.store(true, Ordering::Relaxed);
        self.search_cancel.store(true, Ordering::Relaxed);
        self.scan_generation += 1;
        self.search_generation += 1;
        self.preview_generation += 1;
        let generation = self.scan_generation;
        self.catalog = None;
        self.hits = Arc::new(Vec::new());
        self.selected = None;
        self.total = 0;
        self.preview = Arc::new(Vec::new());
        self.preview_error = None;
        self.error = None;
        self.searching = false;
        self.indexing = !self.settings.folders.is_empty();
        if !self.indexing {
            cx.notify();
            return;
        }
        self.scan_cancel = Arc::new(AtomicBool::new(false));
        self.progress = Arc::new(AtomicUsize::new(0));
        let cancel = self.scan_cancel.clone();
        let progress = self.progress.clone();
        let roots = self.settings.folders.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let catalog = cx
                .background_executor()
                .spawn(async move { search::scan(&roots, &cancel, &progress).map(Arc::new) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.scan_generation != generation {
                    return;
                }
                this.indexing = false;
                this.catalog = catalog;
                if this.catalog.is_none() {
                    this.error = Some("Indexing cancelled. Refresh to try again.".into());
                }
                this.search(cx);
                cx.notify();
            });
        })
        .detach();
    }
    fn search(&mut self, cx: &mut Context<Self>) {
        self.search_cancel.store(true, Ordering::Relaxed);
        self.search_generation += 1;
        self.preview_generation += 1;
        let generation = self.search_generation;
        self.hits = Arc::new(Vec::new());
        self.selected = None;
        self.preview = Arc::new(Vec::new());
        self.preview_error = None;
        self.total = 0;
        let Some(catalog) = self.catalog.clone() else {
            cx.notify();
            return;
        };
        let query = self.input.read(cx).value().to_string();
        if query.trim().is_empty() {
            self.searching = false;
            self.error = None;
            cx.notify();
            return;
        }
        self.error = None;
        self.searching = true;
        self.search_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.search_cancel.clone();
        let scope = self.scope;
        let regex = self.regex;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(180))
                .await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let results = cx
                .background_executor()
                .spawn(async move { catalog.search(&query, scope, regex, &cancel) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.search_generation != generation {
                    return;
                }
                this.searching = false;
                match results {
                    Ok(results) => {
                        this.total = results.total;
                        this.hits = Arc::new(results.hits);
                        if !this.hits.is_empty() {
                            this.select(0, cx);
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(hit) = self.hits.get(index).cloned() else {
            return;
        };
        self.selected = Some(index);
        self.preview_generation += 1;
        let generation = self.preview_generation;
        self.preview = Arc::new(Vec::new());
        self.preview_error = Some("Loading preview...".into());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { search::preview(&hit.path, hit.line) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_generation != generation {
                    return;
                }
                match result {
                    Ok(lines) => {
                        this.preview = Arc::new(lines);
                        this.preview_error = None;
                    }
                    Err(error) => this.preview_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn next(&mut self, cx: &mut Context<Self>) {
        if !self.hits.is_empty() {
            self.select(
                self.selected
                    .map_or(0, |i| (i + 1).min(self.hits.len() - 1)),
                cx,
            );
        }
    }
    fn previous(&mut self, cx: &mut Context<Self>) {
        if !self.hits.is_empty() {
            self.select(self.selected.unwrap_or(0).saturating_sub(1), cx);
        }
    }
    fn current(&self) -> Option<&Hit> {
        self.selected.and_then(|index| self.hits.get(index))
    }
    fn open_file(&mut self, cx: &mut Context<Self>) {
        if let Some(hit) = self.current()
            && let Err(error) = open_path(&hit.path, false)
        {
            self.notice(error, cx);
        }
    }
    fn reveal(&mut self, cx: &mut Context<Self>) {
        if let Some(hit) = self.current()
            && let Err(error) = open_path(&hit.path, true)
        {
            self.notice(error, cx);
        }
    }
    fn copy_path(&mut self, cx: &mut Context<Self>) {
        if let Some(hit) = self.current() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                hit.path.to_string_lossy().into_owned(),
            ));
        }
    }
}
impl Drop for Quarry {
    fn drop(&mut self) {
        self.scan_cancel.store(true, Ordering::Relaxed);
        self.search_cancel.store(true, Ordering::Relaxed);
    }
}
fn open_path(path: &Path, reveal: bool) -> Result<(), String> {
    #[cfg(windows)]
    let normalized = {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        let prefix: Vec<u16> = "\\\\?\\".encode_utf16().collect();
        let unc: Vec<u16> = "\\\\?\\UNC\\".encode_utf16().collect();
        let wide = if wide.starts_with(&unc) {
            [vec![92, 92], wide[unc.len()..].to_vec()].concat()
        } else if wide.starts_with(&prefix) {
            wide[prefix.len()..].to_vec()
        } else {
            wide
        };
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&wide))
    };
    #[cfg(windows)]
    let path = normalized.as_path();
    let mut command = if cfg!(windows) {
        std::process::Command::new("explorer")
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    if reveal && cfg!(windows) {
        let mut argument = std::ffi::OsString::from("/select,");
        argument.push(path);
        command.arg(argument);
    } else if reveal && cfg!(target_os = "macos") {
        command.arg("-R").arg(path);
    } else {
        command.arg(if reveal {
            path.parent().unwrap_or(path)
        } else {
            path
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}
impl Render for Quarry {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let folder_rows = self
            .settings
            .folders
            .iter()
            .enumerate()
            .map(|(i, folder)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(icon(Icon::Folder).color(hsla(p.fg_dim)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .body(text::SM)
                            .child(folder.display().to_string()),
                    )
                    .child(
                        Button::new(("remove-folder", i))
                            .icon(Icon::Close)
                            .ghost()
                            .small()
                            .tooltip("Remove this search folder")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if i >= this.settings.folders.len() {
                                    return;
                                }
                                this.settings.folders.remove(i);
                                if let Err(error) = this.settings.save() {
                                    this.notice(error, cx);
                                }
                                this.scan(cx);
                            })),
                    )
            })
            .collect::<Vec<_>>();
        let folders = panel("Search folders").w(px(230.)).flex_none().min_h_0()
            .child(Button::new("add-folder").label("Add folder").icon(Icon::Plus).primary().full_width().on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_folder(cx))))
            .child(scroll_area("folders-scroll").flex_1().min_h_0().child(div().flex().flex_col().gap_3().children(folder_rows)))
            .child(div().body(text::SM).text_color(hsla(p.fg_dim)).child("Only chosen folders are searched. Hidden files, ignored files and build folders stay out."))
            .child(Button::new("refresh").label("Refresh index").icon(Icon::Refresh).secondary().full_width().disabled(self.settings.folders.is_empty())
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.scan(cx))))
            .when(self.indexing, |panel| panel.child(Button::new("cancel-index").label("Cancel").icon(Icon::Stop).ghost().full_width()
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_scan(cx)))));
        let hits = self.hits.clone();
        let selected = self.selected;
        let weak = cx.weak_entity();
        let list = virtual_list("search-results", hits.len(), move |range, _, cx| {
            let p = palette(cx);
            range
                .map(|i| {
                    let hit = &hits[i];
                    let weak = weak.clone();
                    let name = hit.path.file_name().unwrap_or_default().to_string_lossy();
                    div()
                        .id(("result", i))
                        .h(px(86.))
                        .overflow_hidden()
                        .flex()
                        .flex_col()
                        .justify_center()
                        .gap_1()
                        .px_3()
                        .cursor_pointer()
                        .when(selected == Some(i), |row| row.bg(hsla(p.raised)))
                        .hover(|style| style.bg(hsla(p.surface)))
                        .child(
                            div()
                                .body(text::BASE)
                                .text_color(hsla(if selected == Some(i) { p.accent } else { p.fg }))
                                .child(format!(
                                    "{}{}",
                                    name,
                                    hit.line.map_or(String::new(), |line| format!(" : {line}"))
                                )),
                        )
                        .child(
                            div()
                                .body(text::XS)
                                .text_color(hsla(p.fg_dim))
                                .child(search::snippet(&hit.path.to_string_lossy(), 110)),
                        )
                        .child(div().body(text::SM).text_color(hsla(p.fg_dim)).child(
                            if hit.line.is_some() {
                                hit.text.clone()
                            } else {
                                "Filename match".into()
                            },
                        ))
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| this.select(i, cx));
                        })
                        .into_any_element()
                })
                .collect()
        })
        .size_full();
        let results = if self.hits.is_empty() {
            let (title, hint) = if self.indexing {
                (
                    "Indexing your folders",
                    "You can search as soon as indexing finishes.",
                )
            } else if self.settings.folders.is_empty() {
                (
                    "Find what you need",
                    "Add a folder to search filenames and UTF-8 text locally.",
                )
            } else if self.searching {
                ("Searching", "Finding matches in your index.")
            } else {
                (
                    "No results",
                    "Type a search, change its scope, or refresh after editing files.",
                )
            };
            div()
                .flex_1()
                .child(empty_state(title, hint, window, cx))
                .into_any_element()
        } else {
            list.into_any_element()
        };
        let preview_lines = self.preview.clone();
        let match_line = self.current().and_then(|hit| hit.line);
        let code = virtual_list("preview-lines", preview_lines.len(), move |range, _, cx| {
            let p = palette(cx);
            range
                .map(|i| {
                    let (line, text) = &preview_lines[i];
                    div()
                        .h(px(22.))
                        .flex()
                        .flex_row()
                        .gap_2()
                        .px_2()
                        .body(text::SM)
                        .when(Some(*line) == match_line, |row| row.bg(hsla(p.raised)))
                        .child(
                            div()
                                .w(px(42.))
                                .flex_none()
                                .text_color(hsla(if Some(*line) == match_line {
                                    p.accent
                                } else {
                                    p.fg_faint
                                }))
                                .child(line.to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .child(text.clone()),
                        )
                        .into_any_element()
                })
                .collect()
        })
        .size_full();
        let preview = panel("Preview")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .when_some(self.current(), |panel, hit| {
                panel.meta(
                    hit.path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        Button::new("open-file")
                            .label("Open")
                            .icon(Icon::File)
                            .primary()
                            .small()
                            .disabled(self.current().is_none())
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.open_file(cx)),
                            ),
                    )
                    .child(
                        Button::new("reveal")
                            .label("Reveal")
                            .icon(Icon::Folder)
                            .secondary()
                            .small()
                            .disabled(self.current().is_none())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.reveal(cx))),
                    )
                    .child(
                        Button::new("copy-path")
                            .label("Copy path")
                            .icon(Icon::Copy)
                            .ghost()
                            .small()
                            .disabled(self.current().is_none())
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.copy_path(cx)),
                            ),
                    ),
            )
            .when_some(self.preview_error.as_ref(), |panel, error| {
                panel.child(
                    div()
                        .body(text::SM)
                        .text_color(hsla(p.fg_dim))
                        .child(error.clone()),
                )
            })
            .child(div().flex_1().min_h_0().child(code));
        let bar = toolbar()
            .child(div().flex_1().min_w_0().child(self.input.clone()))
            .child(
                segmented("scope")
                    .option("All")
                    .option("Names")
                    .option("Content")
                    .selected(match self.scope {
                        Scope::All => 0,
                        Scope::Names => 1,
                        Scope::Content => 2,
                    })
                    .on_select(cx.listener(|this, i: &usize, _, cx| {
                        this.scope = [Scope::All, Scope::Names, Scope::Content][*i];
                        this.search(cx);
                    })),
            )
            .child(
                switch("regex")
                    .label("Regex")
                    .checked(self.regex)
                    .on_change(cx.listener(|this, on: &bool, _, cx| {
                        this.regex = *on;
                        this.search(cx);
                    })),
            );
        let files = self.catalog.as_ref().map_or(0, |catalog| catalog.files);
        let summary = self.catalog.as_ref().map(|catalog| {
            format!(
                "{} files / {} text lines / {} skipped / {} errors{}",
                catalog.files,
                catalog.docs.len().saturating_sub(catalog.files),
                catalog.skipped,
                catalog.errors,
                if catalog.capped {
                    " / INDEX LIMIT REACHED"
                } else {
                    ""
                }
            )
        });
        window_frame().child(power_on_in(
            "power",
            div()
                .flex()
                .flex_col()
                .size_full()
                .bg(hsla(p.bg))
                .text_color(hsla(p.fg))
                .body(text::BASE)
                .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                    this.input.update(cx, |input, cx| input.focus(window, cx))
                }))
                .on_action(cx.listener(|this, _: &NextResult, _, cx| this.next(cx)))
                .on_action(cx.listener(|this, _: &PreviousResult, _, cx| this.previous(cx)))
                .on_action(cx.listener(|this, _: &OpenSelected, _, cx| this.open_file(cx)))
                .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                    this.palette
                        .update(cx, |palette, cx| palette.toggle(window, cx))
                }))
                .child(self.palette.clone())
                .child(self.toaster.clone())
                .child(title_bar("Quarry"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_1()
                        .min_h_0()
                        .gap_2()
                        .p_2()
                        .child(folders)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .min_h_0()
                                .gap_2()
                                .child(bar)
                                .when_some(self.error.as_ref(), |view, error| {
                                    view.child(alert("search-error", error.clone()).warning())
                                })
                                .when_some(summary, |view, summary| {
                                    view.child(
                                        div()
                                            .body(text::XS)
                                            .text_color(hsla(p.fg_dim))
                                            .child(summary),
                                    )
                                })
                                .child(
                                    split("results-preview")
                                        .initial(0.48)
                                        .min(px(240.))
                                        .flex_1()
                                        .min_h_0()
                                        .first(
                                            panel("Matches")
                                                .meta(format!(
                                                    "{}{}",
                                                    self.total,
                                                    if self.total > search::MAX_RESULTS {
                                                        " (showing first 500)"
                                                    } else {
                                                        ""
                                                    }
                                                ))
                                                .flex_1()
                                                .min_h_0()
                                                .child(results),
                                        )
                                        .second(preview),
                                ),
                        ),
                )
                .child(
                    status_bar()
                        .left_live(if self.indexing {
                            format!("INDEXING {} FILES", self.progress.load(Ordering::Relaxed))
                        } else {
                            format!("{files} FILES INDEXED")
                        })
                        .right("LOCAL SEARCH")
                        .right(concat!("V", env!("CARGO_PKG_VERSION"))),
                ),
        ))
    }
}
gpui::actions!(
    quarry,
    [FocusSearch, NextResult, PreviousResult, OpenSelected]
);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("ferrite-quarry {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if !args.is_empty() {
        std::process::exit(cli(&args));
    }
    gpui_platform::application().run(|cx: &mut App| {
        ferrite_design::init(Appearance::Dark, cx);
        theme::apply_env(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-shift-p", TogglePalette, None),
            KeyBinding::new("ctrl-f", FocusSearch, None),
            KeyBinding::new("ctrl-j", NextResult, None),
            KeyBinding::new("ctrl-k", PreviousResult, None),
            KeyBinding::new("ctrl-enter", OpenSelected, None),
        ]);
        let options =
            chrome::remembered_window_options("quarry", "Quarry", size(px(1280.), px(820.)), cx);
        cx.open_window(options, |window, cx| {
            chrome::square_corners(window);
            chrome::remember_window("quarry", window, cx);
            chrome::power_off_on_close(window, cx);
            cx.new(|cx| Quarry::new(window, cx))
        })
        .expect("open Quarry window");
        cx.activate(true);
    });
}
fn cli(args: &[String]) -> i32 {
    if args == ["--help"] {
        println!(
            "ferrite-quarry --scan FOLDER --query TEXT [--names | --content] [--regex]\nWithout arguments, open the desktop app."
        );
        return 0;
    }
    let mut roots = Vec::new();
    let mut query = None;
    let mut scope = Scope::All;
    let mut regex = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scan" | "--query" => {
                let Some(value) = args.get(i + 1) else {
                    eprintln!("Missing argument value");
                    return 2;
                };
                if args[i] == "--scan" {
                    roots.push(value.into());
                } else {
                    query = Some(value.clone());
                }
                i += 1;
            }
            "--names" => scope = Scope::Names,
            "--content" => scope = Scope::Content,
            "--regex" => regex = true,
            other => {
                eprintln!("Unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(query) = query.filter(|query| !query.trim().is_empty()) else {
        eprintln!("Use --scan FOLDER --query TEXT");
        return 2;
    };
    if roots.is_empty() {
        eprintln!("Add at least one --scan FOLDER");
        return 2;
    }
    let cancel = AtomicBool::new(false);
    let catalog = search::scan(&roots, &cancel, &AtomicUsize::new(0)).unwrap();
    for warning in &catalog.warnings {
        eprintln!("{warning}");
    }
    if catalog.capped {
        eprintln!("Index limit reached; results cover only scanned files");
    }
    match catalog.search(&query, scope, regex, &cancel) {
        Ok(results) => {
            for hit in results.hits {
                println!(
                    "{}:{}: {}",
                    hit.path.display(),
                    hit.line.unwrap_or(0),
                    hit.text
                );
            }
            eprintln!("{} matches in {} files", results.total, catalog.files);
            if catalog.errors > 0 { 2 } else { 0 }
        }
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}
