//! Maxtrix Portable Organizer — native GUI over the engine.
//!
//! Views (Entdecken / Installiert / Updates / Kategorien) are computed in Rust
//! from a single source-of-truth state and pushed into the Slint model. All state
//! lives on the UI thread; background workers post updates via the event loop.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use maxtrix_engine::catalog::{self, CatalogEntry};
use maxtrix_engine::install::InstallEvent;
use maxtrix_engine::{net, resolver};

mod icons;
mod tray;
mod winsys;
#[cfg(debug_assertions)]
mod testhook;

slint::include_modules!();

thread_local! {
    /// The tray quick-launch window (created on Windows only).
    static MINI: RefCell<Option<slint::Weak<MiniWindow>>> = const { RefCell::new(None) };
    /// Cancel flags of the installs currently running, by tool id.
    static CANCELS: RefCell<HashMap<usize, Arc<AtomicBool>>> = RefCell::new(HashMap::new());
}

/// One tool's authoritative state (source of truth; the model is derived from it).
/// Id offset for full-text search results, kept in a separate id space so the
/// action handlers can tell them apart from catalog/installed tools.
const SEARCH_BASE: usize = 1_000_000;

#[derive(Clone, Default)]
struct ToolState {
    id: usize,
    name: String,
    summary: String,
    category: String,
    version: String, // installed version, else latest once known
    status: String,  // available | installing | installed | update | broken | error | removed | done
    progress: f32,
    detail: String,
    mono: String,
    curated: bool,      // part of the catalog (vs. found on disk / search)
    repo: String,       // search hit: bucket GitHub URL
    file_path: String,  // search hit: manifest path within the repo
    install_cat: String, // folder category on disk (may differ from display category)
    from_status: String, // while installing: the status before (keeps the card in its view)
    icon: slint::Image, // native icon from the tool's .exe, if installed
    has_icon: bool,
    title: String,        // display name ("7-Zip"); `name` stays the Scoop id
    cli: bool,            // command-line tool (console exe / PATH only) → "Terminal"
    dl_size: Option<u64>, // download size of the latest version (catalog check)
    host: String,         // where that download comes from
    disk: Option<u64>,    // space the installed tool takes on the stick
}

/// If the tool is installed, load the icon from its .exe and attach it.
fn attach_icon(t: &mut ToolState, root: &std::path::Path) {
    if t.status != "installed" && t.status != "update" {
        return;
    }
    if let Some(exe) = catalog::tool_executable(root, &folder_cat(t), &t.name) {
        if let Some(img) = icons::icon_for(&exe) {
            t.icon = img;
            t.has_icon = true;
        }
    }
}

/// Reload a single tool's icon on the UI thread (after an install completes).
fn ensure_icon(id: usize) {
    if let Some(root) = with_ctx(|ctx| ctx.state.borrow().root.clone()) {
        update_tool(id, |t| attach_icon(t, &root));
    }
}

struct AppState {
    tools: Vec<ToolState>,
    view: String,
    search: String,
    search_hits: Vec<ToolState>, // full deduped+filtered+sorted list; UI shows `visible` of it
    search_raw: Vec<maxtrix_engine::search::SearchHit>, // everything fetched from the server so far
    search_total: usize,         // server-reported total matches for the query
    search_gen: u64,
    verified_only: bool,
    sort: String,       // "relevanz" | "sterne" | "name"
    page_size: usize,   // 10 | 20 | 50
    visible: usize,     // how many of search_hits to show
    loading_more: bool,
    grid_cols: usize,   // 1 = Liste, 2 = Kacheln
    root: PathBuf,
    lists: Vec<UserList>,  // user-defined collections
    current_list: String,  // selected list (when view == "list")
    discover_cat: String,  // Entdecken filter chip ("" = all)
    open_lists: Vec<String>, // lists expanded in the sidebar
}

/// Group a flat item list into display rows: full-width elements stay single;
/// consecutive plain "tool" cards pack `cols` per row (2 = Kacheln view).
fn rowify(items: Vec<ToolItem>, cols: usize) -> Vec<ToolRow> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < items.len() {
        if cols <= 1 || items[i].kind != "tool" {
            rows.push(ToolRow {
                kind: items[i].kind.clone(),
                a: items[i].clone(),
                b: ToolItem::default(),
                has_b: false,
            });
            i += 1;
        } else {
            let has_b = i + 1 < items.len() && items[i + 1].kind == "tool";
            let b = if has_b { items[i + 1].clone() } else { ToolItem::default() };
            rows.push(ToolRow { kind: "pair".into(), a: items[i].clone(), b, has_b });
            i += if has_b { 2 } else { 1 };
        }
    }
    rows
}

fn push_rows(ui: &MainWindow, mut items: Vec<ToolItem>, cols: usize) {
    // Chapter numbers on the section headers ("01", "02", ...), like the website.
    for (i, it) in items.iter_mut().filter(|it| it.kind == "header").enumerate() {
        it.mono = format!("{:02}", i + 1).into();
    }
    ui.set_rows(ModelRc::from(Rc::new(VecModel::from(rowify(items, cols)))));
}

/// Switch list columns (1/2), persist, refresh.
fn set_cols(n: usize) {
    let n = if n == 2 { 2 } else { 1 };
    with_ctx(|ctx| {
        ctx.state.borrow_mut().grid_cols = n;
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_grid_cols(n as i32);
        }
    });
    save_cols(n);
    refresh();
}

/// Server page size fetched per request (paginated as the user loads more).
const POOL: usize = 50;

/// The folder category on disk for a tool: its own `install_cat` if set, else the
/// display category (curated/found tools store the real category there).
fn folder_cat(t: &ToolState) -> String {
    if t.install_cat.is_empty() { t.category.clone() } else { t.install_cat.clone() }
}

/// Map of lowercased tool name -> (category, version) for everything on the stick.
fn installed_map(root: &std::path::Path) -> std::collections::HashMap<String, (String, String)> {
    catalog::scan_installed(root)
        .into_iter()
        .map(|t| (t.name.to_ascii_lowercase(), (t.category, t.version)))
        .collect()
}

/// Read a tool (from the catalog/installed list or the search results).
fn get_tool(id: usize) -> Option<ToolState> {
    with_ctx(|ctx| {
        let st = ctx.state.borrow();
        if id >= SEARCH_BASE {
            st.search_hits.get(id - SEARCH_BASE).cloned()
        } else {
            st.tools.get(id).cloned()
        }
    })
    .flatten()
}

struct Ctx {
    ui: slint::Weak<MainWindow>,
    state: Rc<RefCell<AppState>>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

fn with_ctx<R>(f: impl FnOnce(&Ctx) -> R) -> Option<R> {
    CTX.with(|c| c.borrow().as_ref().map(f))
}

fn mono(name: &str) -> String {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(2)
        .filter_map(|s| s.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Category → icon kind (chips and section headers). The brand allows no colours,
/// so categories differ by their icon alone.
fn category_icon(cat: &str) -> &'static str {
    match cat {
        "Reverse Engineering" => "chip",
        "Netzwerk" => "network",
        "Sicherheit" => "key",
        "System" => "system",
        "Datenträger" => "disk",
        "Browser" => "globe",
        "Dateien" => "files",
        "Entwicklung" => "cli",
        "Multimedia" => "media",
        "Grafik" => "image",
        _ => "grid",
    }
}

/// The quiet line under a card when there's no status detail: download size and
/// source before an install, the space it takes once it's on the stick.
fn meta_line(t: &ToolState) -> String {
    match t.status.as_str() {
        "installed" | "update" | "broken" => t
            .disk
            .map(|b| format!("{} auf dem Stick", maxtrix_engine::human(b)))
            .unwrap_or_default(),
        "available" | "error" => match (t.dl_size, t.host.is_empty()) {
            (Some(b), false) => format!("Download {} · {}", maxtrix_engine::human(b), t.host),
            (Some(b), true) => format!("Download {}", maxtrix_engine::human(b)),
            (None, false) => format!("von {}", t.host),
            (None, true) => String::new(),
        },
        _ => String::new(),
    }
}

fn title_of(t: &ToolState) -> String {
    if t.title.is_empty() { t.name.clone() } else { t.title.clone() }
}

fn to_item_kind(t: &ToolState, kind: &str) -> ToolItem {
    ToolItem {
        id: t.id as i32,
        kind: kind.into(),
        name: t.name.clone().into(),
        title: title_of(t).into(),
        meta: meta_line(t).into(),
        cli: t.cli,
        version: t.version.clone().into(),
        summary: t.summary.clone().into(),
        category: t.category.clone().into(),
        status: t.status.clone().into(),
        progress: t.progress,
        detail: t.detail.clone().into(),
        mono: t.mono.clone().into(),
        star: false,
        icon: t.icon.clone(),
        has_icon: t.has_icon,
        reinstallable: t.curated || !t.repo.is_empty(),
        ..Default::default()
    }
}

fn to_item(t: &ToolState) -> ToolItem {
    to_item_kind(t, "tool")
}

/// Section header for a category: its icon and how many tools follow.
fn header_item(cat: &str, count: usize) -> ToolItem {
    ToolItem {
        id: -1,
        kind: "header".into(),
        name: cat.into(),
        glyph: category_icon(cat).into(),
        count: count as i32,
        ..Default::default()
    }
}

thread_local! {
    /// Width available for a line of filter chips (logical px; follows the window).
    static CHIP_LINE: Cell<f32> = const { Cell::new(560.0) };
}

/// The content column changed size: re-wrap the chips if a line now holds more
/// or fewer of them (no full list rebuild while the window is being resized).
fn content_resized(width: f32) {
    // Content padding (2 × 18) + room for the scrollbar.
    let line = (width - 36.0 - 16.0).max(360.0);
    if (CHIP_LINE.with(|c| c.get()) - line).abs() < 1.0 {
        return;
    }
    CHIP_LINE.with(|c| c.set(line));
    with_ctx(|ctx| {
        let st = ctx.state.borrow();
        if st.view == "discover" && st.search.is_empty() {
            let curated: Vec<&ToolState> = st.tools.iter().filter(|t| t.curated && t.status != "removed").collect();
            if let Some(ui) = ctx.ui.upgrade() {
                ui.set_chip_rows(ModelRc::from(Rc::new(VecModel::from(chip_rows(&curated, &st.discover_cat)))));
            }
        }
    });
}

/// Filter chips for Entdecken ("Alle" + each category with its count), wrapped
/// into lines that fit the content width (Slint has no flow layout).
fn chip_rows(tools: &[&ToolState], active: &str) -> Vec<ChipRow> {
    let mut cats: Vec<(String, usize)> = Vec::new();
    for t in tools {
        match cats.iter_mut().find(|(c, _)| c == &t.category) {
            Some(e) => e.1 += 1,
            None => cats.push((t.category.clone(), 1)),
        }
    }
    let mut chips = vec![CatItem {
        name: "".into(),
        icon: "grid".into(),
        count: tools.len() as i32,
        active: active.is_empty(),
    }];
    for (c, n) in cats {
        let icon = category_icon(&c);
        chips.push(CatItem { active: c == active, name: c.into(), icon: icon.into(), count: n as i32 });
    }
    // Rough chip width: padding + icon + label + count (12 px Saira ≈ 6.8 px/char).
    let line_width = CHIP_LINE.with(|c| c.get());
    let width = |c: &CatItem| 50.0 + c.name.chars().count().max(4) as f32 * 6.8 + c.count.to_string().len() as f32 * 7.0;
    let mut rows = Vec::new();
    let mut line: Vec<CatItem> = Vec::new();
    let mut used = 0.0;
    for c in chips {
        let w = width(&c);
        if !line.is_empty() && used + w > line_width {
            rows.push(ChipRow { chips: ModelRc::from(Rc::new(VecModel::from(std::mem::take(&mut line)))) });
            used = 0.0;
        }
        used += w + 7.0;
        line.push(c);
    }
    if !line.is_empty() {
        rows.push(ChipRow { chips: ModelRc::from(Rc::new(VecModel::from(line))) });
    }
    rows
}

fn set_discover_cat(cat: String) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        // Clicking the active chip again goes back to "Alle".
        st.discover_cat = if st.discover_cat == cat { String::new() } else { cat };
    });
    refresh();
}

/// A non-tool row (section header / update-all bar).
fn plain_item(kind: &str, name: &str, star: bool) -> ToolItem {
    ToolItem {
        id: -1,
        kind: kind.into(),
        name: name.into(),
        version: "".into(),
        summary: "".into(),
        category: "".into(),
        status: "".into(),
        progress: 0.0,
        detail: "".into(),
        mono: "".into(),
        star,
        ..Default::default()
    }
}

fn view_titles(view: &str, n: usize, updates: usize) -> (String, String) {
    match view {
        "discover" => (
            "Entdecken".into(),
            if n == 1 { "1 kuratiertes Werkzeug · Quelle Scoop · Hash-geprüft".into() } else { format!("{n} kuratierte Werkzeuge · Quelle Scoop · Hash-geprüft") },
        ),
        "installed" => ("Installiert".into(), format!("{n} auf dem Stick")),
        "updates" => (
            "Updates".into(),
            if updates == 0 { "Alles aktuell".into() } else { format!("{updates} verfügbar") },
        ),
        cat => (cat.to_string(), if n == 1 { "1 Werkzeug".into() } else { format!("{n} Werkzeuge") }),
    }
}

/// Recompute the filtered model for the current view/search and push it to the UI.
fn refresh() {
    with_ctx(|ctx| {
        let st = ctx.state.borrow();
        let updates = st.tools.iter().filter(|t| t.status == "update").count();
        let cols = st.grid_cols;
        let busy = st.tools.iter().chain(st.search_hits.iter()).any(|t| t.status == "installing");
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_busy(busy);
            // Member rows show installed state + icons, which change with any status.
            push_lists_ui(&ui, &st);
            ui.set_chip_rows(ModelRc::from(Rc::new(VecModel::from(Vec::<ChipRow>::new()))));
        }

        // Full-text search view renders `visible` of the filtered/sorted results.
        if st.view == "search" {
            let filtered = st.search_hits.len();
            let shown = st.visible.min(filtered);
            let rows: Vec<ToolItem> = st.search_hits.iter().take(shown).map(to_item).collect();
            let more_on_server = st.search_raw.len() < st.search_total;
            let has_more = shown < filtered || more_on_server;
            if let Some(ui) = ctx.ui.upgrade() {
                let sub = if st.search.trim().chars().count() < 2 {
                    "Tippe mindestens 2 Zeichen, um im ganzen Scoop-Katalog zu suchen.".to_string()
                } else {
                    let src = if st.verified_only { "verifiziert" } else { "alle Quellen" };
                    format!("{shown} von {filtered}{} · {src}", if more_on_server { "+" } else { "" })
                };
                ui.set_view_title("Alle durchsuchen".into());
                ui.set_view_subtitle(sub.into());
                ui.set_current_view("search".into());
                ui.set_verified_only(st.verified_only);
                ui.set_search_sort(st.sort.clone().into());
                ui.set_search_page_size(st.page_size as i32);
                ui.set_search_has_more(has_more);
                ui.set_search_loading_more(st.loading_more);
                ui.set_update_count(updates as i32);
                push_rows(&ui, rows, cols);
            }
            return;
        }

        // CLI-Werkzeuge view: its own model (per-tool env + commands).
        if st.view == "cli" {
            let items = build_cli_items(&st.root);
            if let Some(ui) = ctx.ui.upgrade() {
                ui.set_view_title("CLI-Werkzeuge".into());
                ui.set_view_subtitle("Terminal im Tool-Ordner · Tool im PATH · JAVA_HOME etc. gesetzt".into());
                ui.set_current_view("cli".into());
                ui.set_update_count(updates as i32);
                ui.set_cli_tools(ModelRc::from(Rc::new(VecModel::from(items))));
                ui.set_rows(ModelRc::from(Rc::new(VecModel::from(Vec::<ToolRow>::new()))));
            }
            return;
        }

        let search = st.search.to_lowercase();

        // Discover front page (no search): "Empfohlen" feature card + the rest.
        if st.view == "discover" && search.is_empty() {
            let curated: Vec<&ToolState> =
                st.tools.iter().filter(|t| t.curated && t.status != "removed").collect();
            let cat = st.discover_cat.clone();
            let mut rows: Vec<ToolItem> = Vec::new();
            if !cat.is_empty() {
                // One category picked via its chip: just that section.
                let in_cat: Vec<&&ToolState> = curated.iter().filter(|t| t.category == cat).collect();
                rows.push(header_item(&cat, in_cat.len()));
                rows.extend(in_cat.iter().map(|t| to_item(t)));
            } else if let Some((first, rest)) = curated.split_first() {
                rows.push(plain_item("header", "Empfohlen", true));
                rows.push(to_item_kind(first, "feature"));
                // Group the rest under a section header per category (order = first
                // appearance in the catalog).
                let mut cats: Vec<String> = Vec::new();
                for t in rest {
                    if !cats.iter().any(|c| c == &t.category) {
                        cats.push(t.category.clone());
                    }
                }
                for cat in &cats {
                    let n = rest.iter().filter(|t| &t.category == cat).count();
                    rows.push(header_item(cat, n));
                    for t in rest.iter().filter(|t| &t.category == cat) {
                        rows.push(to_item_kind(t, "tool"));
                    }
                }
            }
            if let Some(ui) = ctx.ui.upgrade() {
                let (title, sub) = if cat.is_empty() {
                    view_titles("discover", curated.len(), updates)
                } else {
                    let n = rows.len() - 1;
                    let what = if n == 1 { "1 Werkzeug".to_string() } else { format!("{n} Werkzeuge") };
                    ("Entdecken".into(), format!("{what} in {cat} · Quelle Scoop · Hash-geprüft"))
                };
                ui.set_view_title(title.into());
                ui.set_view_subtitle(sub.into());
                ui.set_chip_rows(ModelRc::from(Rc::new(VecModel::from(chip_rows(&curated, &cat)))));
                ui.set_update_count(updates as i32);
                ui.set_current_view("discover".into());
                push_rows(&ui, rows, cols);
            }
            return;
        }

        // Custom list view: tools whose name is a member of the selected list.
        if st.view == "list" {
            let members: Vec<String> = st
                .lists
                .iter()
                .find(|l| l.name == st.current_list)
                .map(|l| l.tools.clone())
                .unwrap_or_default();
            let mut rows: Vec<ToolItem> = Vec::new();
            for t in &st.tools {
                if t.status != "removed" && members.iter().any(|m| m == &t.name) {
                    rows.push(to_item(t));
                }
            }
            if let Some(ui) = ctx.ui.upgrade() {
                let n = rows.len();
                ui.set_view_title(st.current_list.clone().into());
                ui.set_view_subtitle(if n == 0 {
                    "Noch keine Tools – über das Listen-Symbol auf einer Kachel hinzufügen.".into()
                } else {
                    if n == 1 { "1 Tool in dieser Liste".into() } else { format!("{n} Tools in dieser Liste").into() }
                });
                ui.set_update_count(updates as i32);
                ui.set_current_view("list".into());
                ui.set_current_list(st.current_list.clone().into());
                push_rows(&ui, rows, cols);
            }
            return;
        }

        let mut rows: Vec<ToolItem> = Vec::new();
        for t in &st.tools {
            if t.status == "removed" {
                continue;
            }
            // A running update stays on screen (progress + "Abbrechen") in the
            // view it was started from.
            let status = if t.status == "installing" { t.from_status.as_str() } else { t.status.as_str() };
            let in_view = match st.view.as_str() {
                "discover" => t.curated,
                "installed" => matches!(status, "installed" | "update" | "broken"),
                "updates" => status == "update",
                cat => t.category == cat,
            };
            if !in_view {
                continue;
            }
            if !search.is_empty() {
                let hay = format!("{} {} {} {}", t.name, t.title, t.summary, t.category).to_lowercase();
                if !hay.contains(&search) {
                    continue;
                }
            }
            rows.push(to_item(t));
        }

        // Updates view: lead with the "install all" bar when there is anything to do.
        if st.view == "updates" && updates > 0 {
            rows.insert(0, plain_item("updateall", "", false));
        }

        if let Some(ui) = ctx.ui.upgrade() {
            let (title, sub) = view_titles(&st.view, rows.len(), updates);
            ui.set_view_title(title.into());
            ui.set_view_subtitle(sub.into());
            ui.set_update_count(updates as i32);
            ui.set_current_view(st.view.clone().into());
            push_rows(&ui, rows, cols);
        }
    });
}

/// Mutate a single tool (catalog or search result) by id. A status change can
/// move the tool into or out of the current view, so it rebuilds the list; any
/// other change (progress, detail text, version, icon) only patches that card —
/// a download ticking every percent no longer rebuilds all 50 cards.
fn update_tool(id: usize, f: impl FnOnce(&mut ToolState)) {
    let changed = with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        let t = if id >= SEARCH_BASE {
            st.search_hits.get_mut(id - SEARCH_BASE)
        } else {
            st.tools.iter_mut().find(|t| t.id == id)
        };
        t.map(|t| {
            let before = t.status.clone();
            f(t);
            (before != t.status, t.clone())
        })
    })
    .flatten();
    match changed {
        Some((false, t)) => patch_card(&t),
        _ => refresh(),
    }
}

/// Replace `t`'s card in the rows model that is on screen (no model rebuild).
fn patch_card(t: &ToolState) {
    with_ctx(|ctx| {
        let Some(ui) = ctx.ui.upgrade() else { return };
        let rows = ui.get_rows();
        let id = t.id as i32;
        for i in 0..rows.row_count() {
            let Some(mut row) = rows.row_data(i) else { continue };
            if row.a.id == id {
                row.a = to_item_kind(t, &row.a.kind);
            } else if row.has_b && row.b.id == id {
                row.b = to_item_kind(t, &row.b.kind);
            } else {
                continue;
            }
            rows.set_row_data(i, row);
        }
    });
}

fn do_install(id: usize) {
    let Some(t) = get_tool(id) else { return };
    if t.status == "installing" {
        return;
    }
    let Some((root, ui_weak)) = with_ctx(|ctx| (ctx.state.borrow().root.clone(), ctx.ui.clone())) else {
        return;
    };
    // What the card showed before — restored on cancel, kept on a failed update
    // (the installed version is untouched then and still startable).
    let (prev_status, prev_detail, prev_progress) = (t.status.clone(), t.detail.clone(), t.progress);
    let cancel = Arc::new(AtomicBool::new(false));
    CANCELS.with(|c| c.borrow_mut().insert(id, cancel.clone()));
    let ui_weak2 = ui_weak.clone();
    let name = t.name.clone();
    let repo = t.repo.clone();
    let file_path = t.file_path.clone();
    let from_repo = !repo.is_empty() && !t.curated;
    let is_search = id >= SEARCH_BASE;
    // Folder category on disk: from-repo hits land in "Weitere"; curated tools use
    // their catalog category (even when reached via search).
    let install_cat = if from_repo {
        "Weitere".to_string()
    } else {
        catalog::find(&name).map(|e| e.category.to_string()).unwrap_or_else(|| t.category.clone())
    };

    update_tool(id, |t| {
        t.from_status = std::mem::replace(&mut t.status, "installing".into());
        t.progress = 0.03;
        t.detail = "Vorbereiten…".into();
    });

    std::thread::spawn(move || {
        let last_pct = Cell::new(-1i32);
        let post = move |id: usize, f: Box<dyn FnOnce(&mut ToolState) + Send>| {
            let _ = ui_weak.upgrade_in_event_loop(move |_ui| update_tool(id, f));
        };
        // Re-borrow post via reference so the closures below can call it repeatedly.
        let post = &post;
        let flag = cancel.clone();

        let on = |ev: InstallEvent| match ev {
            // Once cancelled, keep "Wird abgebrochen…" on the card.
            _ if flag.load(Ordering::Relaxed) => {}
            InstallEvent::Download { done, total } => {
                match total.filter(|t| *t > 0) {
                    Some(t) => {
                        let frac = done as f32 / t as f32;
                        let pct = (frac * 100.0) as i32;
                        if pct != last_pct.get() {
                            last_pct.set(pct);
                            post(id, Box::new(move |s| {
                                s.status = "installing".into();
                                s.progress = 0.05 + frac * 0.80;
                                s.detail = format!("Lädt {pct}%");
                            }));
                        }
                    }
                    // Server sent no size: count megabytes instead of a percentage.
                    None => {
                        let mb = (done / (1024 * 1024)) as i32;
                        if mb != last_pct.get() {
                            last_pct.set(mb);
                            post(id, Box::new(move |s| {
                                s.status = "installing".into();
                                s.progress = 0.30;
                                s.detail = format!("Lädt {mb} MB");
                            }));
                        }
                    }
                }
            }
            InstallEvent::Phase(msg) => {
                let m = msg.trim().to_string();
                let (p, d): (f32, Option<String>) = if m.contains("Prüfe") {
                    (0.87, Some("Prüfe Prüfsumme…".into()))
                } else if m.contains("verifiziert") {
                    (0.89, Some("Prüfsumme ok".into()))
                } else if m.starts_with("Entpacke") {
                    (0.91, Some("Entpacke…".into()))
                } else if m.contains("Benutzerdaten") {
                    (0.97, Some("Übernehme Einstellungen…".into()))
                } else if m.starts_with("Plugin") {
                    (0.05, Some(m.clone()))
                } else {
                    (0.0, None)
                };
                if let Some(d) = d {
                    post(id, Box::new(move |s| {
                        s.status = "installing".into();
                        s.progress = p;
                        s.detail = d;
                    }));
                }
            }
        };

        let result = if from_repo {
            catalog::install_from_repo(&name, &repo, &file_path, "Weitere", &root, &on, &cancel)
        } else {
            catalog::install_curated(&name, &root, &on, &cancel)
        };
        let done_flag = cancel.clone();
        let _ = ui_weak2.upgrade_in_event_loop(move |_| {
            CANCELS.with(|c| {
                let mut c = c.borrow_mut();
                if c.get(&id).is_some_and(|f| Arc::ptr_eq(f, &done_flag)) {
                    c.remove(&id);
                }
            })
        });
        // Status to fall back to when nothing changed on disk.
        let unchanged = match prev_status.as_str() {
            "available" | "installed" | "update" | "broken" => prev_status.clone(),
            _ if catalog::installed_version(&root, &install_cat, &name).is_some() => "installed".into(),
            _ => "available".into(),
        };

        match result {
            Ok(summary) => {
                let r = &summary.report;
                let ver = r.version.clone();
                if let Some(reason) = r.pending_reason.clone() {
                    let reason = short_err(&reason);
                    post(id, Box::new(move |s| {
                        s.status = "error".into();
                        s.progress = 0.0;
                        s.detail = reason;
                    }));
                } else {
                    let files = r.files;
                    let extra = if summary.plugins.is_empty() {
                        String::new()
                    } else {
                        format!(" · + {}", summary.plugins.join(", "))
                    };
                    let files_txt = if files == 1 { "1 Datei".to_string() } else { format!("{files} Dateien") };
                    let detail = format!("{files_txt}{extra}"); // "installiert" zeigt die Status-Pille
                    let ver_s = ver.clone();
                    let cat_s = install_cat.clone();
                    let disk = catalog::dir_size(&r.target);
                    let cli = catalog::is_console_tool(&root, &install_cat, &name);
                    let done_msg = if prev_status == "update" {
                        format!("{} auf v{ver} aktualisiert", catalog::display_name(&name))
                    } else {
                        format!("{} installiert", catalog::display_name(&name))
                    };
                    let _ = ui_weak2.upgrade_in_event_loop(move |_| show_toast(done_msg, "ok"));
                    let name_v = name.clone();
                    let ver_v = ver.clone();
                    post(id, Box::new(move |s| {
                        s.status = "installed".into();
                        s.progress = 1.0;
                        s.version = ver_s;
                        s.install_cat = cat_s;
                        s.detail = detail;
                        s.disk = Some(disk);
                        s.cli = cli;
                    }));
                    let _ = ui_weak2.upgrade_in_event_loop(move |_| record_installed_version(&name_v, &ver_v));
                    // A search-installed tool isn't in the main list yet — add it so
                    // "Installiert" (and a re-search) reflect it without a restart.
                    if is_search {
                        let name_m = name.clone();
                        let cat_m = install_cat.clone();
                        let ver_m = ver.clone();
                        let _ = ui_weak2
                            .upgrade_in_event_loop(move |_| merge_found_tool(&name_m, &cat_m, &ver_m));
                    } else {
                        let _ = ui_weak2.upgrade_in_event_loop(|_| refresh_drive_card());
                    }
                    let _ = ui_weak2.upgrade_in_event_loop(move |_| ensure_icon(id));
                }
            }
            Err(e) if e.downcast_ref::<maxtrix_engine::Cancelled>().is_some() => {
                // Nothing was changed on disk — the card goes back to how it was.
                let restore_detail = if unchanged == prev_status { prev_detail } else { String::new() };
                let installed = unchanged != "available";
                let msg = if installed { "Update abgebrochen – nichts verändert" } else { "Installation abgebrochen" };
                let _ = ui_weak2.upgrade_in_event_loop(move |_| show_toast(msg, "info"));
                post(id, Box::new(move |s| {
                    s.status = unchanged;
                    s.progress = if installed { prev_progress.max(1.0) } else { 0.0 };
                    s.detail = restore_detail;
                }));
            }
            Err(e) => {
                let err = short_err(&e.to_string());
                // A failed update leaves the old version in place: keep it startable.
                let (status, detail) = match unchanged.as_str() {
                    "update" => ("update".to_string(), format!("Update fehlgeschlagen: {err}")),
                    "broken" => ("broken".to_string(), format!("Fehler: {err}")),
                    _ => ("error".to_string(), format!("Fehler: {err}")),
                };
                let msg = format!("{}: {}", catalog::display_name(&name), if status == "update" { "Update fehlgeschlagen" } else { "Installation fehlgeschlagen" });
                let _ = ui_weak2.upgrade_in_event_loop(move |_| show_toast(msg, "error"));
                post(id, Box::new(move |s| {
                    s.status = status;
                    s.progress = 0.0;
                    s.detail = detail;
                }));
            }
        }
    });
}

/// Ask a running install to stop (at the next download chunk / step). The engine
/// leaves the installed version untouched, so nothing needs to be rolled back.
fn cancel_install(id: usize) {
    let flag = CANCELS.with(|c| c.borrow().get(&id).cloned());
    if let Some(flag) = flag {
        flag.store(true, Ordering::Relaxed);
        update_tool(id, |t| t.detail = "Wird abgebrochen…".into());
    }
}

fn do_launch(id: usize) {
    let Some(t) = get_tool(id) else { return };
    let root = with_ctx(|ctx| ctx.state.borrow().root.clone()).unwrap_or_default();
    if let Err(e) = catalog::launch_tool(&root, &folder_cat(&t), &t.name) {
        let err = short_err(&e.to_string());
        show_toast(format!("{} startet nicht: {err}", title_of(&t)), "error");
        update_tool(id, |t| t.detail = format!("Start-Fehler: {err}"));
    }
}

/// "Terminal" on a command-line tool: a shell in its folder with it on PATH.
fn open_terminal_for(id: usize) {
    let Some(t) = get_tool(id) else { return };
    let root = with_ctx(|ctx| ctx.state.borrow().root.clone()).unwrap_or_default();
    if let Err(e) = maxtrix_engine::cli::open_tool_terminal(&root, &folder_cat(&t), &t.name) {
        show_toast(format!("Terminal: {}", short_err(&e.to_string())), "error");
    }
}

/// A destructive action waiting for the user's confirmation. (Deleting a list
/// needs none — it can be undone from the toast.)
enum Pending {
    RemoveTool(usize),
}

thread_local! {
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

fn ask_confirm(pending: Pending, title: String, text: String, button: &str) {
    PENDING.with(|p| *p.borrow_mut() = Some(pending));
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_confirm_title(title.into());
            ui.set_confirm_text(text.into());
            ui.set_confirm_button(button.into());
            ui.set_confirm_open(true);
        }
    });
}

/// "Entfernen" deletes the whole tool folder incl. the user data inside → ask first.
fn request_remove(id: usize) {
    let Some(t) = get_tool(id) else { return };
    let folder = format!("Tools\\{}\\{}", folder_cat(&t), t.name);
    ask_confirm(
        Pending::RemoveTool(id),
        format!("{} entfernen?", title_of(&t)),
        format!(
            "Der Ordner {folder} wird vom Stick gelöscht – samt allen Einstellungen und Daten, \
             die das Tool dort gespeichert hat. Das lässt sich nicht rückgängig machen."
        ),
        "Entfernen",
    );
}

fn confirm_accept() {
    match PENDING.with(|p| p.borrow_mut().take()) {
        Some(Pending::RemoveTool(id)) => do_remove(id),
        None => {}
    }
}

fn do_remove(id: usize) {
    let Some(t) = get_tool(id) else { return };
    let root = with_ctx(|ctx| ctx.state.borrow().root.clone()).unwrap_or_default();
    let name = t.name.clone();
    match catalog::remove_tool(&root, &folder_cat(&t), &name) {
        Ok(()) => {
            update_tool(id, |t| {
                // Curated + search entries can be re-installed → "available"; a plain
                // found-on-disk tool has no source, so it disappears.
                t.status = if t.curated || id >= SEARCH_BASE { "available".into() } else { "removed".into() };
                t.progress = 0.0;
                t.detail = String::new();
            });
            // Drop the found-on-disk copy from the main list so "Installiert" updates.
            drop_found_tool(&name);
            show_toast(format!("{} entfernt", title_of(&t)), "ok");
        }
        Err(e) => update_tool(id, |t| t.detail = format!("Entfernen fehlgeschlagen: {}", short_err(&e.to_string()))),
    }
}

fn do_open(id: usize) {
    let Some(t) = get_tool(id) else { return };
    let root = with_ctx(|ctx| ctx.state.borrow().root.clone()).unwrap_or_default();
    let _ = catalog::open_in_explorer(&root, &folder_cat(&t), &t.name);
}

/// Add a just-installed (non-curated) tool to the main list so "Installiert" shows
/// it immediately, without waiting for a restart/rescan.
fn merge_found_tool(name: &str, category: &str, version: &str) {
    with_ctx(|ctx| {
        let root = ctx.state.borrow().root.clone();
        let mut st = ctx.state.borrow_mut();
        if let Some(t) = st.tools.iter_mut().find(|t| t.name.eq_ignore_ascii_case(name)) {
            t.status = "installed".into();
            t.version = version.to_string();
            t.install_cat = category.to_string();
            t.cli = catalog::is_console_tool(&root, category, name);
            attach_icon(t, &root);
            return;
        }
        let id = st.tools.len();
        let mut t = ToolState {
            id,
            name: name.to_string(),
            summary: "über die Suche installiert".into(),
            title: catalog::display_name(name),
            cli: catalog::is_console_tool(&root, category, name),
            category: category.to_string(),
            version: version.to_string(),
            status: "installed".into(),
            progress: 1.0,
            detail: String::new(),
            mono: mono(name),
            curated: false,
            repo: String::new(),
            file_path: String::new(),
            install_cat: category.to_string(),
            ..Default::default()
        };
        attach_icon(&mut t, &root);
        st.tools.push(t);
    });
    refresh();
    refresh_drive_card();
}

/// Remove a found-on-disk copy of `name` from the main list (after an uninstall).
fn drop_found_tool(name: &str) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        st.tools.retain(|t| t.curated || !t.name.eq_ignore_ascii_case(name));
        // A curated tool that was removed goes back to available.
        if let Some(t) = st.tools.iter_mut().find(|t| t.curated && t.name.eq_ignore_ascii_case(name)) {
            if matches!(t.status.as_str(), "installed" | "update" | "broken") {
                t.status = "available".into();
                t.version = String::new();
            }
        }
    });
    refresh();
    refresh_drive_card();
}

/// Install every tool currently flagged as having an update.
fn do_update_all() {
    let ids: Vec<usize> = with_ctx(|ctx| {
        ctx.state
            .borrow()
            .tools
            .iter()
            .filter(|t| t.status == "update")
            .map(|t| t.id)
            .collect()
    })
    .unwrap_or_default();
    for id in ids {
        do_install(id);
    }
}

/// Bar fraction + caption for the sidebar drive card.
fn drive_cap_props(usage: Option<(u64, u64)>, n_installed: usize) -> (f32, String) {
    let tools = if n_installed == 1 { "1 Tool".to_string() } else { format!("{n_installed} Tools") };
    match usage {
        Some((free, total)) if total > 0 => {
            let used = total.saturating_sub(free);
            (
                used as f32 / total as f32,
                format!("{} frei · {tools}", maxtrix_engine::human(free)),
            )
        }
        _ => (0.0, tools),
    }
}

/// Number of tools that occupy space on the stick (installed or updatable).
fn installed_count(tools: &[ToolState]) -> usize {
    tools
        .iter()
        .filter(|t| matches!(t.status.as_str(), "installed" | "update" | "broken"))
        .count()
}

/// Recompute the sidebar drive card (free space + tool count) from current state.
fn refresh_drive_card() {
    with_ctx(|ctx| {
        let (root, n) = {
            let st = ctx.state.borrow();
            (st.root.clone(), installed_count(&st.tools))
        };
        let (frac, cap) = drive_cap_props(maxtrix_engine::drives::disk_usage(&root), n);
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_drive_fraction(frac);
            ui.set_drive_cap(cap.into());
        }
    });
}

/// Condense a possibly multi-line engine error into one short card-friendly line.
fn short_err(s: &str) -> String {
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s).trim();
    const MAX: usize = 90;
    if first.chars().count() > MAX {
        let clipped: String = first.chars().take(MAX).collect();
        format!("{clipped}…")
    } else {
        first.to_string()
    }
}


/// Open the detail drawer for a tool; fetch source/hash/deps/size in the background.
fn open_details(id: usize) {
    let Some(t) = get_tool(id) else { return };
    let Some(ui_weak) = with_ctx(|ctx| ctx.ui.clone()) else { return };

    let name = t.name.clone();
    let repo = t.repo.clone();
    let file_path = t.file_path.clone();
    let from_repo = !repo.is_empty() && !t.curated;
    let install_cat = if from_repo { "Weitere".to_string() } else { t.category.clone() };

    let action = match t.status.as_str() {
        "installed" if t.cli => "Terminal öffnen",
        "installed" => "Starten",
        "update" => "Auf neueste Version aktualisieren",
        "broken" => "Neu installieren",
        "installing" | "done" => "",
        _ => "Herunterladen & Installieren",
    };

    let Some(ui) = ui_weak.upgrade() else { return };
    ui.set_drawer_id(id as i32);
    ui.set_drawer_open(true);
    ui.set_drawer_name(title_of(&t).into());
    ui.set_drawer_version(t.version.clone().into());
    ui.set_drawer_category(t.category.clone().into());
    ui.set_drawer_summary(t.summary.clone().into());
    ui.set_drawer_mono(t.mono.clone().into());
    ui.set_drawer_icon(t.icon.clone());
    ui.set_drawer_has_icon(t.has_icon);
    ui.set_drawer_target(format!("Tools\\{install_cat}\\{name}\\").into());
    ui.set_drawer_action(action.into());
    ui.set_drawer_deps(ModelRc::from(Rc::new(VecModel::from(Vec::<slint::SharedString>::new()))));

    // A tool found on disk (not curated, not a search result) has no manifest.
    if !t.curated && !from_repo {
        ui.set_drawer_loading(false);
        ui.set_drawer_source("lokal – nicht aus dem Katalog".into());
        ui.set_drawer_hash("—".into());
        ui.set_drawer_size("—".into());
        return;
    }
    ui.set_drawer_loading(true);
    ui.set_drawer_source("".into());
    ui.set_drawer_hash("".into());
    ui.set_drawer_size("wird ermittelt…".into());

    std::thread::spawn(move || {
        let mut source = String::new();
        let mut hash = String::new();
        let mut ver = String::new();
        let mut deps: Vec<slint::SharedString> = Vec::new();
        let mut total: u64 = 0;
        let mut size_known = false;

        let manifest = if from_repo {
            catalog::fetch_manifest_from_repo(&repo, &file_path, &name)
        } else {
            catalog::fetch_named(&name).map(|(m, _)| m)
        };

        if let Ok(m) = manifest {
            source = m.url.clone();
            hash = format!("{}:{}", m.hash_alg, m.hash_hex);
            ver = m.version.clone();
            if let Some(sz) = net::head_size(&m.url) {
                total += sz;
                size_known = true;
            }
            for need in resolver::plugin_needs(&m) {
                match catalog::fetch_ref(&need.reference) {
                    Ok(dep) => {
                        deps.push(format!("{} v{} · wird automatisch mitinstalliert", dep.name, dep.version).into());
                        if let Some(sz) = net::head_size(&dep.url) {
                            total += sz;
                        }
                    }
                    Err(_) => deps.push(format!("{} · automatisch", need.reference).into()),
                }
            }
        }
        let size = if size_known { maxtrix_engine::human(total) } else { "unbekannt".into() };

        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            if ui.get_drawer_open() && ui.get_drawer_id() == id as i32 {
                if !ver.is_empty() {
                    ui.set_drawer_version(ver.into());
                }
                ui.set_drawer_source(source.into());
                ui.set_drawer_hash(hash.into());
                ui.set_drawer_deps(ModelRc::from(Rc::new(VecModel::from(deps))));
                ui.set_drawer_size(size.into());
                ui.set_drawer_loading(false);
            }
        });
    });
}

/// Build a ToolState for a search hit, marking it installed if already on the stick.
fn build_search_state(
    id: usize,
    h: &maxtrix_engine::search::SearchHit,
    installed: &std::collections::HashMap<String, (String, String)>,
    root: &std::path::Path,
) -> ToolState {
    let label = h.bucket_label();
    let chip = if h.official {
        label.rsplit('/').next().unwrap_or(&label).to_string()
    } else {
        label.split('/').next().unwrap_or(&label).to_string()
    };
    let provenance = if h.stars > 0 {
        format!("  —  {}★{}", h.stars, if h.official { " · offiziell" } else { "" })
    } else {
        String::new()
    };
    let curated = catalog::find(&h.name).is_some();
    // Already on the stick? Reflect that so the search shows Starten/Entfernen.
    let on_disk = installed.get(&h.name.to_ascii_lowercase());
    let (status, version, install_cat) = match on_disk {
        Some((cat, ver)) => (
            "installed".to_string(),
            if ver.is_empty() { h.version.clone() } else { ver.clone() },
            cat.clone(),
        ),
        None => {
            let cat = catalog::find(&h.name)
                .map(|e| e.category.to_string())
                .unwrap_or_else(|| "Weitere".to_string());
            ("available".to_string(), h.version.clone(), cat)
        }
    };
    let cli = status == "installed" && catalog::is_console_tool(root, &install_cat, &h.name);
    let mut state = ToolState {
        id,
        name: h.name.clone(),
        title: catalog::display_name(&h.name),
        cli,
        summary: format!("{}{}", h.description, provenance),
        category: chip,
        version,
        status,
        progress: 0.0,
        detail: String::new(),
        mono: mono(&h.name),
        curated,
        repo: h.repo.clone(),
        file_path: h.file_path.clone(),
        install_cat,
        ..Default::default()
    };
    attach_icon(&mut state, root);
    state
}

/// Rebuild the full search list from the raw pool: dedup by name, apply the
/// verified filter, sort by the chosen key, mark on-disk tools installed. No
/// network. Then refresh (which slices to `visible`).
fn apply_search_filter() {
    with_ctx(|ctx| {
        let (raw, verified, sort, query, root) = {
            let st = ctx.state.borrow();
            (st.search_raw.clone(), st.verified_only, st.sort.clone(), st.search.trim().to_ascii_lowercase(), st.root.clone())
        };

        // Dedup by name (keep first occurrence = server relevance order).
        let mut seen = std::collections::HashSet::new();
        let mut hits: Vec<&maxtrix_engine::search::SearchHit> = raw
            .iter()
            .filter(|h| seen.insert(h.name.to_ascii_lowercase()))
            .filter(|h| {
                if !verified {
                    return true;
                }
                // Official bucket, but not "Nonportable" (deliberately system installers).
                h.official && !h.bucket_label().to_ascii_lowercase().ends_with("/nonportable")
            })
            .collect();

        // Stable sort keeps server order as the tie-break.
        match sort.as_str() {
            "sterne" => hits.sort_by(|a, b| b.stars.cmp(&a.stars)),
            "name" => hits.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase())),
            _ => {
                // Relevanz: float an exact/prefix name match up, else server order.
                let rank = |h: &maxtrix_engine::search::SearchHit| {
                    let nl = h.name.to_ascii_lowercase();
                    if nl == query { 2 } else if nl.starts_with(&query) { 1 } else { 0 }
                };
                hits.sort_by(|a, b| rank(b).cmp(&rank(a)));
            }
        }

        let map = installed_map(&root);
        let states: Vec<ToolState> = hits
            .iter()
            .enumerate()
            .map(|(i, h)| build_search_state(SEARCH_BASE + i, h, &map, &root))
            .collect();
        ctx.state.borrow_mut().search_hits = states;
    });
    refresh();
}

/// Start a fresh full-text search (page 0); apply only if still the latest query.
fn run_search(query: String) {
    let query = query.trim().to_string();
    let Some((gen, ui_weak)) = with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        st.search_gen += 1;
        st.visible = st.page_size; // new query -> back to the first page
        (st.search_gen, ctx.ui.clone())
    }) else {
        return;
    };

    if query.chars().count() < 2 {
        with_ctx(|ctx| {
            let mut st = ctx.state.borrow_mut();
            st.search_raw.clear();
            st.search_hits.clear();
            st.search_total = 0;
        });
        refresh();
        return;
    }

    std::thread::spawn(move || {
        let page = maxtrix_engine::search::search_page(&query, 0, POOL).ok();
        let _ = ui_weak.upgrade_in_event_loop(move |_ui| {
            let apply = with_ctx(|ctx| {
                let mut st = ctx.state.borrow_mut();
                if st.search_gen != gen {
                    return false;
                }
                match page {
                    Some(p) => {
                        st.search_raw = p.hits;
                        st.search_total = p.total;
                    }
                    None => {
                        st.search_raw.clear();
                        st.search_total = 0;
                    }
                }
                true
            })
            .unwrap_or(false);
            if apply {
                apply_search_filter();
            }
        });
    });
}

/// "Mehr anzeigen": reveal the next page; fetch another server page if the loaded
/// pool is exhausted but more matches exist.
fn load_more() {
    let Some((need_fetch, gen, query, skip, ui_weak)) = with_ctx(|ctx| {
        let st = ctx.state.borrow();
        let filtered = st.search_hits.len();
        let need_fetch = st.visible >= filtered && st.search_raw.len() < st.search_total;
        (need_fetch, st.search_gen, st.search.trim().to_string(), st.search_raw.len(), ctx.ui.clone())
    }) else {
        return;
    };

    if !need_fetch {
        // Just reveal more of what we already have.
        with_ctx(|ctx| {
            let mut st = ctx.state.borrow_mut();
            st.visible += st.page_size;
        });
        refresh();
        return;
    }

    // Fetch the next server page in the background.
    with_ctx(|ctx| ctx.state.borrow_mut().loading_more = true);
    refresh();
    std::thread::spawn(move || {
        let page = maxtrix_engine::search::search_page(&query, skip, POOL).ok();
        let _ = ui_weak.upgrade_in_event_loop(move |_ui| {
            let apply = with_ctx(|ctx| {
                let mut st = ctx.state.borrow_mut();
                st.loading_more = false;
                if st.search_gen != gen {
                    return false;
                }
                if let Some(p) = page {
                    st.search_raw.extend(p.hits);
                    if p.total > 0 {
                        st.search_total = p.total;
                    }
                }
                st.visible += st.page_size;
                true
            })
            .unwrap_or(false);
            if apply {
                apply_search_filter();
            } else {
                refresh();
            }
        });
    });
}

/// Change the sort key and jump back to the first page.
fn set_sort(sort: String) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        st.sort = sort;
        st.visible = st.page_size;
    });
    apply_search_filter();
}

/// Change the page size and jump back to the first page.
fn set_page_size(n: usize) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        st.page_size = n;
        st.visible = n;
    });
    apply_search_filter();
}

fn drawer_close() {
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_drawer_open(false);
        }
    });
}

fn drawer_primary() {
    let id = with_ctx(|ctx| {
        let ui = ctx.ui.upgrade();
        let id = ui.as_ref().map(|u| u.get_drawer_id()).unwrap_or(-1);
        if let Some(u) = &ui {
            u.set_drawer_open(false);
        }
        id
    })
    .unwrap_or(-1);
    if id < 0 {
        return;
    }
    let id = id as usize;
    let (status, cli) = get_tool(id).map(|t| (t.status, t.cli)).unwrap_or_default();
    if status == "installed" && cli {
        open_terminal_for(id);
        return;
    }
    if id >= SEARCH_BASE {
        match status.as_str() {
            "installed" => do_launch(id),
            "installing" | "done" => {}
            _ => do_install(id),
        }
        return;
    }
    match status.as_str() {
        "installed" => do_launch(id),
        "update" => do_install(id),
        "installing" | "done" => {}
        _ => do_install(id),
    }
}

// ---- Stick root: detection, selection, persistence ----

fn app_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_path() -> Option<PathBuf> {
    Some(app_dir().join(".maxtrix-config.json"))
}

fn config_backup_path() -> PathBuf {
    app_dir().join(".maxtrix-config.bak.json")
}

fn parse_config_file(p: &std::path::Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(p).ok()?;
    serde_json::from_str::<serde_json::Value>(&text).ok().filter(|v| v.is_object())
}

/// The config, or — if it is unreadable (e.g. the stick was pulled mid-write) —
/// the last good backup, so lists and settings don't silently vanish.
fn read_config() -> serde_json::Value {
    config_path()
        .and_then(|p| parse_config_file(&p))
        .or_else(|| parse_config_file(&config_backup_path()))
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Replace the config atomically: write a temp file, flush it to the device, keep
/// the previous good version as backup, then rename. An unreadable file is moved
/// aside instead of being overwritten.
fn write_config(cfg: &serde_json::Value) {
    let Some(p) = config_path() else { return };
    let tmp = p.with_extension("json.tmp");
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(cfg.to_string().as_bytes())?;
        f.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if p.exists() {
        if parse_config_file(&p).is_some() {
            let _ = std::fs::copy(&p, config_backup_path());
        } else {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = std::fs::rename(&p, app_dir().join(format!(".maxtrix-config.defekt-{stamp}.json")));
        }
    }
    let _ = std::fs::rename(&tmp, &p);
}

/// Set one field in the config, preserving the others.
fn write_config_field(key: &str, val: serde_json::Value) {
    let mut cfg = read_config();
    cfg[key] = val;
    write_config(&cfg);
}

fn load_saved_root() -> Option<PathBuf> {
    let root = PathBuf::from(read_config()["root"].as_str()?);
    root.is_dir().then_some(root)
}

fn save_root(root: &std::path::Path) {
    write_config_field("root", serde_json::json!(root.to_string_lossy()));
}

/// Persisted list columns (1 = Liste, 2 = Kacheln). Default 1.
fn load_cols() -> usize {
    match read_config()["cols"].as_u64() {
        Some(2) => 2,
        _ => 1,
    }
}

fn save_cols(n: usize) {
    write_config_field("cols", serde_json::json!(n));
}

// ── User-defined lists (custom collections of tools) ──

#[derive(Clone)]
struct UserList {
    name: String,
    tools: Vec<String>, // tool names (any tool: curated or installed)
}

fn load_lists() -> Vec<UserList> {
    read_config()["lists"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let name = v["name"].as_str()?.to_string();
                    let tools = v["tools"]
                        .as_array()
                        .map(|t| t.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    Some(UserList { name, tools })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_lists(lists: &[UserList]) {
    let arr: Vec<serde_json::Value> = lists
        .iter()
        .map(|l| serde_json::json!({ "name": l.name, "tools": l.tools }))
        .collect();
    write_config_field("lists", serde_json::json!(arr));
}

/// Sidebar model: each list with its members (shown when the list is open).
fn list_items(st: &AppState) -> Vec<ListItem> {
    st.lists
        .iter()
        .map(|l| {
            let tools: Vec<ListTool> = l
                .tools
                .iter()
                .map(|name| match st.tools.iter().find(|t| t.name.eq_ignore_ascii_case(name) && t.status != "removed") {
                    Some(t) => ListTool {
                        id: t.id as i32,
                        name: t.name.clone().into(),
                        title: title_of(t).into(),
                        mono: t.mono.clone().into(),
                        icon: t.icon.clone(),
                        has_icon: t.has_icon,
                        installed: matches!(t.status.as_str(), "installed" | "update"),
                    },
                    // Removed from the stick and not in the catalog: keep the entry visible.
                    None => ListTool {
                        id: -1,
                        name: name.clone().into(),
                        title: catalog::display_name(name).into(),
                        mono: mono(name).into(),
                        icon: slint::Image::default(),
                        has_icon: false,
                        installed: false,
                    },
                })
                .collect();
            ListItem {
                name: l.name.clone().into(),
                count: l.tools.len() as i32,
                open: st.open_lists.iter().any(|o| o == &l.name),
                tools: ModelRc::from(Rc::new(VecModel::from(tools))),
            }
        })
        .collect()
}

/// Popup model: every list + whether `tool` is a member (for the check marks).
fn list_toggles_for(tool: &str, lists: &[UserList]) -> Vec<ListToggle> {
    lists
        .iter()
        .map(|l| ListToggle {
            name: l.name.clone().into(),
            active: l.tools.iter().any(|t| t == tool),
        })
        .collect()
}

fn push_lists_ui(ui: &MainWindow, st: &AppState) {
    ui.set_user_lists(ModelRc::from(Rc::new(VecModel::from(list_items(st)))));
}

/// Re-push the sidebar lists after a change (and the list view, if it shows one).
fn lists_changed() {
    let in_list_view = with_ctx(|ctx| {
        let st = ctx.state.borrow();
        save_lists(&st.lists);
        if let Some(ui) = ctx.ui.upgrade() {
            push_lists_ui(&ui, &st);
        }
        st.view == "list"
    })
    .unwrap_or(false);
    if in_list_view {
        refresh();
    }
}

fn load_open_lists() -> Vec<String> {
    read_config()["lists_open"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Expand/collapse a list in the sidebar (remembered across starts).
fn toggle_list_open(name: String) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        if let Some(pos) = st.open_lists.iter().position(|o| o == &name) {
            st.open_lists.remove(pos);
        } else {
            st.open_lists.push(name);
        }
        write_config_field("lists_open", serde_json::json!(st.open_lists));
        drop(st);
        if let Some(ui) = ctx.ui.upgrade() {
            push_lists_ui(&ui, &ctx.state.borrow());
        }
    });
}

/// A tool clicked inside an open sidebar list: start it (terminal for CLI tools);
/// not installed → its details, so it can be installed from there.
fn list_tool_clicked(id: i32) {
    if id < 0 {
        show_toast("Nicht mehr auf dem Stick und nicht im Katalog", "info");
        return;
    }
    let id = id as usize;
    let Some(t) = get_tool(id) else { return };
    match t.status.as_str() {
        "installed" | "update" if t.cli => open_terminal_for(id),
        "installed" | "update" => {
            do_launch(id);
            show_toast(format!("{} gestartet", title_of(&t)), "ok");
        }
        _ => open_details(id),
    }
}

fn remove_from_list(tool: String, list: String) {
    with_ctx(|ctx| {
        if let Some(l) = ctx.state.borrow_mut().lists.iter_mut().find(|l| l.name == list) {
            l.tools.retain(|t| t != &tool);
        }
    });
    lists_changed();
    show_toast(format!("{} aus „{list}“ entfernt", catalog::display_name(&tool)), "ok");
}

fn create_list(name: String) {
    let name = name.trim().to_string();
    if name.is_empty() {
        return;
    }
    // A tool that goes straight in (dropped on "Neue Liste", or from its popup).
    let tool_id = with_ctx(|ctx| ctx.ui.upgrade().map(|u| u.get_new_list_tool_id()).unwrap_or(-1)).unwrap_or(-1);
    let tool = (tool_id >= 0).then(|| get_tool(tool_id as usize)).flatten();
    let created = with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        if st.lists.iter().any(|l| l.name.eq_ignore_ascii_case(&name)) {
            return false;
        }
        let tools = tool.as_ref().map(|t| vec![t.name.clone()]).unwrap_or_default();
        st.lists.push(UserList { name: name.clone(), tools });
        if !st.open_lists.contains(&name) {
            st.open_lists.push(name.clone());
            write_config_field("lists_open", serde_json::json!(st.open_lists));
        }
        true
    })
    .unwrap_or(false);
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_new_list_open(false);
            ui.set_new_list_name("".into());
            ui.set_new_list_tool_id(-1);
            ui.set_new_list_tool_title("".into());
        }
    });
    if created {
        lists_changed();
        match &tool {
            Some(t) => {
                show_toast(format!("{} → „{name}“ (neue Liste)", title_of(t)), "ok");
                flash_list(&name);
            }
            None => show_toast(format!("Liste „{name}“ angelegt"), "ok"),
        }
    } else {
        show_toast(format!("Eine Liste „{name}“ gibt es schon"), "info");
    }
}

/// Delete a list right away — the toast offers "Rückgängig" instead of asking first.
fn delete_list(name: String) {
    let removed = with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        let pos = st.lists.iter().position(|l| l.name == name)?;
        let list = st.lists.remove(pos);
        if st.current_list == name && st.view == "list" {
            st.current_list = String::new();
            st.view = "discover".into();
        }
        Some((list, pos))
    })
    .flatten();
    let Some((list, pos)) = removed else { return };
    lists_changed();
    refresh();
    toast_with(format!("Liste „{name}“ gelöscht"), "info", "Rückgängig");
    UNDO.with(|u| *u.borrow_mut() = Some(Undo::DeletedList(list, pos)));
}

fn toast_action() {
    match UNDO.with(|u| u.borrow_mut().take()) {
        Some(Undo::DeletedList(list, pos)) => {
            let name = list.name.clone();
            with_ctx(|ctx| {
                let mut st = ctx.state.borrow_mut();
                let pos = pos.min(st.lists.len());
                st.lists.insert(pos, list);
            });
            lists_changed();
            show_toast(format!("Liste „{name}“ wiederhergestellt"), "ok");
        }
        None => hide_toast(),
    }
}

fn select_list(name: String) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        st.view = "list".into();
        st.current_list = name;
    });
    refresh();
}

/// Open the add-to-list popup for a tool (by id), with per-list check state.
fn open_list_menu(id: usize) {
    let Some(t) = get_tool(id) else { return };
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            let st = ctx.state.borrow();
            let toggles = list_toggles_for(&t.name, &st.lists);
            ui.set_list_menu_tool(t.name.clone().into());
            ui.set_list_menu_title(title_of(&t).into());
            ui.set_list_menu_id(id as i32);
            ui.set_list_menu_toggles(ModelRc::from(Rc::new(VecModel::from(toggles))));
            ui.set_list_menu_open(true);
        }
    });
}

/// Toggle a tool's membership in a list (from the popup).
fn toggle_list(tool: String, list: String) {
    let added = with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        let l = st.lists.iter_mut().find(|l| l.name == list)?;
        Some(match l.tools.iter().position(|t| t == &tool) {
            Some(pos) => {
                l.tools.remove(pos);
                false
            }
            None => {
                l.tools.push(tool.clone());
                true
            }
        })
    })
    .flatten();
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            let st = ctx.state.borrow();
            ui.set_list_menu_toggles(ModelRc::from(Rc::new(VecModel::from(list_toggles_for(&tool, &st.lists)))));
        }
    });
    lists_changed();
    match added {
        Some(true) => {
            show_toast(format!("{} → „{list}“", catalog::display_name(&tool)), "ok");
            flash_list(&list);
        }
        Some(false) => show_toast(format!("{} aus „{list}“ entfernt", catalog::display_name(&tool)), "ok"),
        None => {}
    }
}

/// Drag & drop drop target: resolve the dragged tool by id (captured at pointer-down,
/// so it's reliable even if the row model rebuilds mid-drag), then add its name.
fn drop_on_list_id(id: usize, list: String) {
    if let Some(t) = get_tool(id) {
        drop_on_list(t.name, list);
    }
}

/// Dropped on "Neue Liste mit diesem Tool": ask for the name, the tool goes in.
fn drop_new_list(id: i32) {
    let Some(t) = (id >= 0).then(|| get_tool(id as usize)).flatten() else { return };
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_new_list_name("".into());
            ui.set_new_list_tool_id(id);
            ui.set_new_list_tool_title(title_of(&t).into());
            ui.set_new_list_open(true);
        }
    });
}

/// Add a tool to a list (drag & drop drop target; add-only, no toggle-off).
fn drop_on_list(tool: String, list: String) {
    let added = with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        let l = st.lists.iter_mut().find(|l| l.name == list)?;
        if l.tools.iter().any(|t| t == &tool) {
            return Some(false);
        }
        l.tools.push(tool.clone());
        Some(true)
    })
    .flatten();
    lists_changed();
    match added {
        Some(true) => show_toast(format!("{} → „{list}“", catalog::display_name(&tool)), "ok"),
        Some(false) => show_toast(format!("{} ist schon in „{list}“", catalog::display_name(&tool)), "info"),
        None => return,
    }
    flash_list(&list);
}

// ---- Toasts ----

thread_local! {
    static TOAST_GEN: Cell<u64> = const { Cell::new(0) };
    static UNDO: RefCell<Option<Undo>> = const { RefCell::new(None) };
    static FLASH_GEN: Cell<u64> = const { Cell::new(0) };
}

/// What the toast's "Rückgängig" restores.
enum Undo {
    DeletedList(UserList, usize),
}

fn show_toast(text: impl Into<String>, kind: &str) {
    toast_with(text, kind, "");
}

/// Show a toast for a few seconds (longer when it offers an action). A newer
/// toast replaces an older one; an "undo" is only offered while its toast shows.
fn toast_with(text: impl Into<String>, kind: &str, action: &str) {
    let text = text.into();
    let gen = TOAST_GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    UNDO.with(|u| *u.borrow_mut() = None);
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_toast_text(text.into());
            ui.set_toast_kind(kind.into());
            ui.set_toast_action(action.into());
            ui.set_toast_visible(true);
        }
    });
    let ms = if action.is_empty() { 3200 } else { 6500 };
    slint::Timer::single_shot(std::time::Duration::from_millis(ms), move || {
        if TOAST_GEN.with(|g| g.get()) == gen {
            hide_toast();
        }
    });
}

fn hide_toast() {
    UNDO.with(|u| *u.borrow_mut() = None);
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_toast_visible(false);
        }
    });
}

/// Let the list a tool just went into light up briefly.
fn flash_list(name: &str) {
    let gen = FLASH_GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_flash_list(name.into());
        }
    });
    slint::Timer::single_shot(std::time::Duration::from_millis(900), move || {
        if FLASH_GEN.with(|g| g.get()) == gen {
            with_ctx(|ctx| {
                if let Some(ui) = ctx.ui.upgrade() {
                    ui.set_flash_list("".into());
                }
            });
        }
    });
}

/// Root at startup: a previously chosen root if still valid; else, if the app is
/// not itself on a stick and exactly one removable drive exists, that drive; else
/// the app's own directory.
fn initial_root() -> PathBuf {
    if let Some(saved) = load_saved_root() {
        return saved;
    }
    let app = app_dir();
    let removable = maxtrix_engine::drives::list_removable();
    let app_key = app.to_string_lossy().trim_end_matches(['\\', '/']).to_ascii_uppercase();
    let on_removable = removable
        .iter()
        .any(|d| app_key.starts_with(&d.root.trim_end_matches(['\\', '/']).to_ascii_uppercase()));
    if !on_removable && removable.len() == 1 {
        return PathBuf::from(&removable[0].root);
    }
    app
}

/// Human label for the current root (a removable drive's label, else the path).
fn drive_display(root: &std::path::Path) -> String {
    let rp = root.to_string_lossy();
    let key = rp.trim_end_matches(['\\', '/']);
    for d in maxtrix_engine::drives::list_removable() {
        if key.eq_ignore_ascii_case(d.root.trim_end_matches(['\\', '/'])) {
            return d.display();
        }
    }
    rp.into_owned()
}

/// Build the tool list for a root: curated (with installed detection) + tools found on disk.
fn build_tools(root: &std::path::Path) -> Vec<ToolState> {
    let mut tools: Vec<ToolState> = catalog::curated()
        .iter()
        .enumerate()
        .map(|(id, e): (usize, &CatalogEntry)| {
            let installed = catalog::installed_version(root, e.category, e.name);
            ToolState {
                id,
                name: e.name.into(),
                title: e.title.into(),
                summary: e.summary.into(),
                category: e.category.into(),
                version: installed.clone().unwrap_or_default(),
                status: if installed.is_some() { "installed".into() } else { "available".into() },
                progress: if installed.is_some() { 1.0 } else { 0.0 },
                detail: String::new(),
                mono: mono(e.name),
                curated: true,
                repo: String::new(),
                file_path: String::new(),
                install_cat: e.category.into(),
                ..Default::default()
            }
        })
        .collect();

    for it in catalog::scan_installed(root) {
        if it.curated {
            continue;
        }
        tools.push(ToolState {
            id: tools.len(),
            name: it.name.clone(),
            title: it.name.clone(),
            summary: "auf dem Stick gefunden".into(),
            category: it.category.clone(),
            version: it.version.clone(),
            status: "installed".into(),
            progress: 1.0,
            detail: String::new(),
            mono: mono(&it.name),
            curated: false,
            repo: String::new(),
            file_path: String::new(),
            install_cat: it.category.clone(),
            ..Default::default()
        });
    }
    for t in &mut tools {
        // An install whose launch target is missing (e.g. an older version only
        // saved a setup .msi) must not pretend to be startable.
        if t.status == "installed" {
            if let Some(missing) = catalog::broken_launch_target(root, &folder_cat(t), &t.name) {
                t.status = "broken".into();
                t.detail = format!("Unvollständig – '{missing}' fehlt");
            }
        }
        if t.status == "installed" {
            t.cli = catalog::is_console_tool(root, &folder_cat(t), &t.name);
        }
        attach_icon(t, root);
    }
    tools
}

/// Build the CLI-view model: each curated command-line tool with its env lines,
/// suggested commands, and installed state.
fn build_cli_items(root: &std::path::Path) -> Vec<CliToolItem> {
    maxtrix_engine::cli::cli_tools()
        .iter()
        .filter_map(|t| {
            let id = catalog::curated().iter().position(|e| e.name == t.name)? as i32;
            let cat = t.category();
            let installed = catalog::installed_version(root, cat, t.name).is_some();
            let mut env: Vec<slint::SharedString> = vec![format!("PATH  +=  Tools\\{cat}\\{}", t.name).into()];
            env.extend(t.env.iter().map(|e| (*e).into()));
            let cmds: Vec<slint::SharedString> = t.commands.iter().map(|c| (*c).into()).collect();
            let icon = if installed {
                catalog::tool_executable(root, cat, t.name).and_then(|exe| icons::icon_for(&exe))
            } else {
                None
            };
            let title = catalog::display_name(t.name);
            Some(CliToolItem {
                id,
                mono: mono(&title).into(),
                name: title.into(),
                path: format!("Tools\\{cat}\\{}\\", t.name).into(),
                installed,
                env: ModelRc::from(Rc::new(VecModel::from(env))),
                commands: ModelRc::from(Rc::new(VecModel::from(cmds))),
                has_icon: icon.is_some(),
                icon: icon.unwrap_or_default(),
            })
        })
        .collect()
}

/// How long fetched upstream versions count as current (no network at start).
const VERSION_TTL_SECS: u64 = 3 * 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the catalog check knows about a tool's latest version.
#[derive(Clone, Default)]
struct Latest {
    version: String,
    size: Option<u64>,
    host: String,
}

/// Latest upstream info, cached next to the app: `(fetched_at, name → info)`.
fn version_cache_path() -> PathBuf {
    app_dir().join(".maxtrix-versions.json")
}

fn read_version_cache() -> Option<(u64, HashMap<String, Latest>)> {
    let text = std::fs::read_to_string(version_cache_path()).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut map: HashMap<String, Latest> = HashMap::new();
    if let Some(tools) = json["tools"].as_object() {
        for (k, v) in tools {
            map.insert(
                k.clone(),
                Latest {
                    version: v["version"].as_str().unwrap_or("").to_string(),
                    size: v["size"].as_u64(),
                    host: v["host"].as_str().unwrap_or("").to_string(),
                },
            );
        }
    } else if let Some(versions) = json["versions"].as_object() {
        // Cache format of Build 2026-09-29-b (versions only).
        for (k, v) in versions {
            if let Some(v) = v.as_str() {
                map.insert(k.clone(), Latest { version: v.to_string(), ..Default::default() });
            }
        }
    }
    Some((json["fetched"].as_u64().unwrap_or(0), map))
}

fn write_version_cache(fetched: u64, tools: &HashMap<String, Latest>) {
    let tools: serde_json::Map<String, serde_json::Value> = tools
        .iter()
        .map(|(k, l)| (k.clone(), serde_json::json!({ "version": l.version, "size": l.size, "host": l.host })))
        .collect();
    let json = serde_json::json!({ "fetched": fetched, "tools": tools });
    let path = version_cache_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(&json) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// A fresh install *is* the latest version — keep the cache in line, so a cached
/// older "latest" can't flag it as needing an update.
fn record_installed_version(name: &str, version: &str) {
    if catalog::find(name).is_none() || version.is_empty() {
        return;
    }
    let (fetched, mut map) = read_version_cache().unwrap_or((0, HashMap::new()));
    map.entry(name.to_string()).or_default().version = version.to_string();
    write_version_cache(fetched, &map);
}

/// Apply known latest versions in one go: fill in versions, download sizes and
/// sources, flag (or clear) updates — then a single list rebuild.
fn apply_latest(latest: &HashMap<String, Latest>) {
    with_ctx(|ctx| {
        let mut st = ctx.state.borrow_mut();
        for t in st.tools.iter_mut().filter(|t| t.curated) {
            let Some(l) = latest.get(&t.name) else { continue };
            if l.size.is_some() {
                t.dl_size = l.size;
            }
            if !l.host.is_empty() {
                t.host = l.host.clone();
            }
            let v = &l.version;
            if v.is_empty() {
                continue;
            }
            match t.status.as_str() {
                "available" => t.version = v.clone(),
                "installed" | "update" => {
                    if !t.version.is_empty() && &t.version != v {
                        t.status = "update".into();
                        t.detail = format!("Update: v{} → v{}", t.version, v);
                    } else if t.status == "update" {
                        t.status = "installed".into();
                        t.detail.clear();
                    }
                }
                _ => {}
            }
        }
    });
    refresh();
}

/// Fetch every curated manifest (version, size, source) with a few parallel workers.
fn fetch_latest_parallel() -> HashMap<String, Latest> {
    let entries = catalog::curated();
    let next = AtomicUsize::new(0);
    let out = Mutex::new(HashMap::new());
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(e) = entries.get(i) else { break };
                if let Ok(info) = catalog::latest_info(e.name) {
                    let l = Latest { version: info.version, size: info.size, host: info.host };
                    out.lock().unwrap().insert(e.name.to_string(), l);
                }
            });
        }
    });
    out.into_inner().unwrap_or_default()
}

/// Version check: the last known versions are shown at once (instant "Updates"
/// badge after start); the network is only asked when they are older than the TTL.
fn spawn_version_fetch() {
    let cached = read_version_cache();
    if let Some((_, map)) = &cached {
        apply_latest(map);
    }
    // A cache from before sizes were recorded is refreshed right away.
    let has_sizes = cached.as_ref().is_some_and(|(_, m)| m.values().any(|l| l.size.is_some()));
    if has_sizes && cached.as_ref().is_some_and(|(ts, _)| now_secs().saturating_sub(*ts) < VERSION_TTL_SECS) {
        return;
    }
    let Some(ui_weak) = with_ctx(|ctx| ctx.ui.clone()) else { return };
    std::thread::spawn(move || {
        let fresh = fetch_latest_parallel();
        if fresh.is_empty() {
            return; // offline — keep what we have
        }
        let (old_ts, mut merged) = cached.unwrap_or((0, HashMap::new()));
        // Only a (nearly) complete check resets the clock; otherwise retry next start.
        let complete = fresh.len() * 10 >= catalog::curated().len() * 9;
        merged.extend(fresh);
        write_version_cache(if complete { now_secs() } else { old_ts }, &merged);
        let _ = ui_weak.upgrade_in_event_loop(move |_| apply_latest(&merged));
    });
}

/// Space each installed tool takes on the stick (a folder walk — in the
/// background, one batched update when done).
fn spawn_disk_sizes() {
    let Some((root, items, ui_weak)) = with_ctx(|ctx| {
        let st = ctx.state.borrow();
        let items: Vec<(usize, String, String)> = st
            .tools
            .iter()
            .filter(|t| matches!(t.status.as_str(), "installed" | "update" | "broken"))
            .map(|t| (t.id, folder_cat(t), t.name.clone()))
            .collect();
        (st.root.clone(), items, ctx.ui.clone())
    }) else {
        return;
    };
    std::thread::spawn(move || {
        let sizes: Vec<(usize, u64)> = items
            .iter()
            .map(|(id, cat, name)| (*id, catalog::dir_size(&catalog::install_dir(&root, cat, name))))
            .collect();
        let _ = ui_weak.upgrade_in_event_loop(move |_| {
            with_ctx(|ctx| {
                let mut st = ctx.state.borrow_mut();
                for (id, size) in sizes {
                    if let Some(t) = st.tools.iter_mut().find(|t| t.id == id) {
                        t.disk = Some(size);
                    }
                }
            });
            refresh();
        });
    });
}

/// Switch the stick root: persist it, rebuild the tool list + categories, refresh.
fn set_root(new_root: PathBuf) {
    save_root(&new_root);
    maxtrix_engine::fsutil::cleanup_stale(&new_root);
    let tools = build_tools(&new_root);
    let (frac, cap) = drive_cap_props(maxtrix_engine::drives::disk_usage(&new_root), installed_count(&tools));
    with_ctx(|ctx| {
        {
            let mut st = ctx.state.borrow_mut();
            st.root = new_root.clone();
            st.tools = tools;
            st.search_hits.clear();
        }
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_drive_info(drive_display(&new_root).into());
            ui.set_drive_fraction(frac);
            ui.set_drive_cap(cap.into());
            ui.set_picker_open(false);
        }
    });
    refresh();
    spawn_version_fetch();
    spawn_disk_sizes();
}

fn open_picker() {
    let drives = maxtrix_engine::drives::list_removable();
    let app = app_dir().to_string_lossy().into_owned();
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            let opts: Vec<DriveOption> = drives
                .iter()
                .map(|d| DriveOption {
                    label: d.display().into(),
                    path: d.root.clone().into(),
                })
                .collect();
            ui.set_drives(ModelRc::from(Rc::new(VecModel::from(opts))));
            ui.set_app_folder(app.clone().into());
            ui.set_picker_open(true);
        }
    });
}

fn do_pick_drive(path: String) {
    if !path.is_empty() {
        set_root(PathBuf::from(path));
    }
}

fn do_pick_folder() {
    match rfd::FileDialog::new().set_title("Stick-Wurzel wählen").pick_folder() {
        Some(dir) => set_root(dir),
        None => {
            with_ctx(|ctx| {
                if let Some(ui) = ctx.ui.upgrade() {
                    ui.set_picker_open(false);
                }
            });
        }
    }
}

fn close_picker() {
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_picker_open(false);
        }
    });
}

// ---- Tray + quick-launch mini window ----

/// Build the mini-window model: installed tools grouped by category (header rows
/// + tool rows with name + icon).
fn build_mini_items() -> Vec<MiniItem> {
    with_ctx(|ctx| {
        let st = ctx.state.borrow();
        let mut installed: Vec<&ToolState> = st
            .tools
            .iter()
            .filter(|t| t.status == "installed" || t.status == "update")
            .collect();
        installed.sort_by(|a, b| {
            a.category
                .to_ascii_lowercase()
                .cmp(&b.category.to_ascii_lowercase())
                .then(title_of(a).to_ascii_lowercase().cmp(&title_of(b).to_ascii_lowercase()))
        });
        let mut items: Vec<MiniItem> = Vec::new();
        let mut cur = String::new();
        for t in installed {
            if t.category != cur {
                cur = t.category.clone();
                items.push(MiniItem {
                    kind: "header".into(),
                    id: -1,
                    name: cur.clone().into(),
                    mono: "".into(),
                    icon: slint::Image::default(),
                    has_icon: false,
                });
            }
            items.push(MiniItem {
                kind: "tool".into(),
                id: t.id as i32,
                name: title_of(t).into(),
                mono: t.mono.clone().into(),
                icon: t.icon.clone(),
                has_icon: t.has_icon,
            });
        }
        items
    })
    .unwrap_or_default()
}

fn refresh_mini() {
    let items = build_mini_items();
    MINI.with(|m| {
        if let Some(mini) = m.borrow().as_ref().and_then(|w| w.upgrade()) {
            mini.set_tools(ModelRc::from(Rc::new(VecModel::from(items))));
        }
    });
}

/// Bring the main window back (from the tray).
pub(crate) fn show_main_window() {
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            let _ = ui.show();
            ui.window().set_minimized(false);
            #[cfg(windows)]
            apply_dark_frame(ui.as_weak(), true);
        }
    });
}

/// Give a component's window the dark frame once its native window exists (the
/// event loop creates it, so retry briefly); optionally bring it to the front.
#[cfg(windows)]
fn apply_dark_frame<C: ComponentHandle + 'static>(weak: slint::Weak<C>, focus: bool) {
    fn attempt<C: ComponentHandle + 'static>(weak: slint::Weak<C>, focus: bool, tries: u32) {
        let Some(c) = weak.upgrade() else { return };
        if winsys::dark_frame(c.window()) {
            if focus {
                winsys::focus(c.window());
            }
        } else if tries > 0 {
            slint::Timer::single_shot(std::time::Duration::from_millis(40), move || {
                attempt(weak, focus, tries - 1)
            });
        }
    }
    attempt(weak, focus, 25);
}

/// Finish opening the mini window once its native window exists (retry briefly):
/// dark frame, bottom-right above the taskbar, in front of the main window.
#[cfg(windows)]
fn settle_mini(weak: slint::Weak<MiniWindow>, tries: u32) {
    let Some(mini) = weak.upgrade() else { return };
    if winsys::place_bottom_right(mini.window()) {
        winsys::dark_frame(mini.window());
        winsys::focus(mini.window());
    } else if tries > 0 {
        slint::Timer::single_shot(std::time::Duration::from_millis(40), move || settle_mini(weak, tries - 1));
    }
}

/// Bring the main window up with the "Über" dialog open.
#[cfg(windows)]
pub(crate) fn show_about() {
    show_main_window();
    with_ctx(|ctx| {
        if let Some(ui) = ctx.ui.upgrade() {
            ui.set_about_open(true);
        }
    });
}

/// Show the quick-launch mini window (rebuild its list, place bottom-right).
pub(crate) fn show_mini_window() {
    refresh_mini();
    MINI.with(|m| {
        if let Some(mini) = m.borrow().as_ref().and_then(|w| w.upgrade()) {
            let _ = mini.show();
            #[cfg(windows)]
            settle_mini(mini.as_weak(), 25);
        }
    });
}

pub(crate) fn toggle_mini_window() {
    let visible = MINI
        .with(|m| {
            m.borrow()
                .as_ref()
                .and_then(|w| w.upgrade())
                .map(|mini| mini.window().is_visible())
        })
        .unwrap_or(false);
    if visible {
        MINI.with(|m| {
            if let Some(mini) = m.borrow().as_ref().and_then(|w| w.upgrade()) {
                let _ = mini.hide();
            }
        });
    } else {
        show_mini_window();
    }
}

pub(crate) fn quit_app() {
    let _ = slint::quit_event_loop();
}

/// What the window's X does once the user decided: "tray" or "quit" (None = ask).
pub(crate) fn close_action() -> Option<String> {
    read_config()["close_action"].as_str().map(str::to_string)
}

/// "Einfache Grafik" (software renderer) — takes effect at the next start.
pub(crate) fn soft_render_setting() -> bool {
    read_config()["renderer"].as_str() == Some("software")
}

pub(crate) fn set_soft_render(on: bool) {
    write_config_field("renderer", serde_json::json!(if on { "software" } else { "auto" }));
}

pub(crate) fn set_close_to_tray(tray: bool) {
    write_config_field("close_action", serde_json::json!(if tray { "tray" } else { "quit" }));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    let instance = match winsys::acquire_instance() {
        winsys::Start::AlreadyRunning => return Ok(()),
        winsys::Start::First(guard) => guard,
    };

    // Rescue for PCs whose GPU driver leaves the window blank: the tray menu can
    // switch to the software renderer (read before the first window exists).
    if soft_render_setting() && std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
    let ui = MainWindow::new()?;

    let root = initial_root();
    // Work folders an interrupted install left behind (killed app, pulled stick).
    maxtrix_engine::fsutil::cleanup_stale(&root);
    let tools = build_tools(&root);
    let (frac, cap) = drive_cap_props(maxtrix_engine::drives::disk_usage(&root), installed_count(&tools));
    ui.set_drive_info(drive_display(&root).into());
    ui.set_drive_fraction(frac);
    ui.set_drive_cap(cap.into());
    ui.set_grid_cols(load_cols() as i32);
    let lists = load_lists();
    ui.set_build_tag(format!("v{} · Build {}", env!("CARGO_PKG_VERSION"), env!("MAXTRIX_BUILD")).into());
    let soft = std::env::var("SLINT_BACKEND").is_ok_and(|b| b.contains("software"));
    ui.global::<Render>().set_soft(soft);

    // Optional deep-link to a starting view (also handy for testing).
    let initial_view = std::env::var("MAXTRIX_VIEW")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "discover".into());

    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            ui: ui.as_weak(),
            state: Rc::new(RefCell::new(AppState {
                tools,
                view: initial_view,
                search: String::new(),
                search_hits: Vec::new(),
                search_raw: Vec::new(),
                search_total: 0,
                search_gen: 0,
                verified_only: true,
                sort: "relevanz".into(),
                page_size: 20,
                visible: 20,
                loading_more: false,
                grid_cols: load_cols(),
                root: root.clone(),
                lists,
                current_list: String::new(),
                discover_cat: String::new(),
                open_lists: load_open_lists(),
            })),
        })
    });
    refresh();

    // Wire callbacks (all run on the UI thread).
    ui.on_install(|id| do_install(id as usize));
    ui.on_update(|id| do_install(id as usize)); // update == reinstall latest
    ui.on_cancel(|id| cancel_install(id as usize));
    ui.on_update_all(do_update_all);
    ui.on_launch(|id| do_launch(id as usize));
    ui.on_remove(|id| request_remove(id as usize));
    ui.on_confirm_accept(confirm_accept);
    ui.on_open(|id| do_open(id as usize));
    ui.on_show_details(|id| open_details(id as usize));
    ui.on_drawer_primary(drawer_primary);
    ui.on_drawer_close(drawer_close);
    ui.on_open_console(|| {
        if let Some(root) = with_ctx(|ctx| ctx.state.borrow().root.clone()) {
            if let Err(e) = maxtrix_engine::cli::open_console(&root) {
                show_toast(format!("Konsole: {}", short_err(&e.to_string())), "error");
            }
        }
    });
    // Cards and the CLI view both pass tool ids (curated ids == catalog index).
    ui.on_open_terminal(|id| open_terminal_for(id as usize));
    ui.on_copy_cmd(|c| match maxtrix_engine::cli::copy_to_clipboard(&c) {
        Ok(()) => show_toast("Befehl kopiert – im Terminal einfügen", "ok"),
        Err(e) => show_toast(format!("Kopieren fehlgeschlagen: {e}"), "error"),
    });
    ui.on_toggle_verified(|| {
        with_ctx(|ctx| {
            let mut st = ctx.state.borrow_mut();
            st.verified_only = !st.verified_only;
            st.visible = st.page_size;
        });
        apply_search_filter();
    });
    ui.on_set_sort(|s| set_sort(s.to_string()));
    ui.on_set_page_size(|n| set_page_size(n as usize));
    ui.on_load_more(load_more);
    ui.on_set_cols(|n| set_cols(n as usize));
    ui.on_create_list(|n| create_list(n.to_string()));
    ui.on_delete_list(|n| delete_list(n.to_string()));
    ui.on_toggle_list_open(|n| toggle_list_open(n.to_string()));
    ui.on_list_tool_clicked(list_tool_clicked);
    ui.on_remove_from_list(|t, l| remove_from_list(t.to_string(), l.to_string()));
    ui.on_drop_new_list(drop_new_list);
    ui.on_toast_action_clicked(toast_action);
    ui.on_set_discover_cat(|c| set_discover_cat(c.to_string()));
    ui.on_content_resized(content_resized);
    ui.on_select_list(|n| select_list(n.to_string()));
    ui.on_open_list_menu(|id| open_list_menu(id as usize));
    ui.on_toggle_list(|tool, list| toggle_list(tool.to_string(), list.to_string()));
    ui.on_drop_on_list_id(|id, list| drop_on_list_id(id as usize, list.to_string()));
    ui.on_open_picker(open_picker);
    ui.on_pick_drive(|p| do_pick_drive(p.to_string()));
    ui.on_pick_folder(do_pick_folder);
    ui.on_close_picker(close_picker);
    ui.on_select_view(|v| {
        let (view, search) = with_ctx(|ctx| {
            let mut st = ctx.state.borrow_mut();
            st.view = v.to_string();
            (st.view.clone(), st.search.clone())
        })
        .unwrap_or_default();
        if view == "search" {
            run_search(search);
        } else {
            refresh();
        }
    });
    ui.on_search_changed(|s| {
        let is_search = with_ctx(|ctx| {
            let mut st = ctx.state.borrow_mut();
            st.search = s.to_string();
            st.view == "search"
        })
        .unwrap_or(false);
        if is_search {
            run_search(s.to_string());
        } else {
            refresh();
        }
    });

    spawn_version_fetch();
    spawn_disk_sizes();

    // Debug builds only: scripted UI test run (see testhook.rs).
    #[cfg(debug_assertions)]
    if let Ok(script) = std::env::var("MAXTRIX_SCRIPT") {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || testhook::run(weak, script));
    }

    // Optional deep-link: open a tool's detail drawer at startup (testing/linking).
    if let Ok(open_name) = std::env::var("MAXTRIX_OPEN") {
        if let Some(id) = catalog::curated().iter().position(|e| e.name == open_name) {
            open_details(id);
        }
    }
    // Optional deep-link: open the stick picker at startup (testing).
    if std::env::var("MAXTRIX_PICKER").is_ok() {
        open_picker();
    }
    // Optional deep-link: run a full-text search at startup (testing/linking).
    if let Ok(q) = std::env::var("MAXTRIX_SEARCH") {
        if !q.is_empty() {
            with_ctx(|ctx| {
                let mut st = ctx.state.borrow_mut();
                st.view = "search".into();
                st.search = q.clone();
            });
            run_search(q);
        }
    }

    // On Windows: closing the main window hides it to the system tray; the app
    // keeps running with a tray icon + quick-launch mini window. Other platforms
    // use the normal run-until-last-window-closed behaviour.
    #[cfg(windows)]
    {
        let mini = MiniWindow::new()?;
        mini.global::<Render>().set_soft(std::env::var("SLINT_BACKEND").is_ok_and(|b| b.contains("software")));
        mini.on_launch(|id| do_launch(id as usize));
        mini.on_open_main(|| {
            show_main_window();
        });
        mini.window()
            .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
        MINI.with(|m| *m.borrow_mut() = Some(mini.as_weak()));

        ui.show()?;
        apply_dark_frame(ui.as_weak(), false);
        let _instance_timer = {
            let t = slint::Timer::default();
            t.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(250), move || {
                if instance.show_requested() {
                    show_main_window();
                }
            });
            t
        };
        let tray_guard: Option<tray::TrayGuard> = tray::setup();
        // Testing helper: open the mini window straight away.
        if std::env::var("MAXTRIX_MINI").is_ok() {
            show_mini_window();
        }
        // Debug-only: screenshot of the mini window (see testhook.rs).
        #[cfg(debug_assertions)]
        if let Ok(path) = std::env::var("MAXTRIX_MINI_SHOT") {
            show_mini_window();
            let weak = mini.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
                if let Some(m) = weak.upgrade() {
                    testhook::save_snapshot(m.window(), std::path::Path::new(&path));
                }
            });
        }
        if tray_guard.is_some() {
            // Tray present: closing the main window hides it there — or quits, if the
            // user chose so. The first close asks once.
            ui.window().on_close_requested(|| match close_action().as_deref() {
                Some("quit") => {
                    quit_app();
                    slint::CloseRequestResponse::HideWindow
                }
                Some(_) => slint::CloseRequestResponse::HideWindow,
                None => {
                    with_ctx(|ctx| {
                        if let Some(ui) = ctx.ui.upgrade() {
                            ui.set_close_ask_open(true);
                        }
                    });
                    slint::CloseRequestResponse::KeepWindowShown
                }
            });
            ui.on_close_to_tray(|| {
                set_close_to_tray(true);
                with_ctx(|ctx| {
                    if let Some(ui) = ctx.ui.upgrade() {
                        let _ = ui.hide();
                    }
                });
            });
            ui.on_close_quit(|| {
                set_close_to_tray(false);
                quit_app();
            });
            slint::run_event_loop_until_quit()?;
        } else {
            // No tray icon — don't strand an invisible process; quit on close.
            slint::run_event_loop()?;
        }
    }
    #[cfg(not(windows))]
    {
        ui.run()?;
    }
    Ok(())
}
