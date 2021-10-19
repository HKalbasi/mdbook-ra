use std::{collections::HashMap, fs, path::PathBuf, process::Command, sync::Arc};

use ide::{AnalysisHost, Change, FileId, InlayKind, TextSize};
use rust_analyzer::cli::load_cargo::{load_workspace_at, LoadCargoConfig};
use toml::Value;
use vfs::VfsPath;

use super::*;

/// A no-op preprocessor.
pub struct Nop;

impl Nop {
    pub fn new() -> Nop {
        Nop
    }
}

const CODE_START: &str = "```rust";
const CODE_END: &str = "```";

fn run_on_codes<F: FnMut(&str) -> String>(text: &str, mut run_on_code: F) -> String {
    let mut result = String::new();
    let mut iter = text.split(CODE_START);
    result += r#"


<style>
    .inlay-hint, .inlay-hint * {
        background-color: #444;
        color: #999;
        border-radius: .4em;
    }
    .inlay-hint {
        font-size: 0.8em;
        user-select: none;
    }
    .hover-holder {
        max-height: 40vh;
        overflow: auto;
    }
</style>
"#;
    result += iter.next().unwrap();
    for x in iter {
        result += r#"<pre><code class="language-rust hljs">"#;
        let (code, content) = x.split_once(CODE_END).unwrap();
        result += &run_on_code(code);
        result += "</code></pre>";
        result += content;
    }
    result
}

fn raw_code(code: &str) -> String {
    let mut result = "".to_string();
    for l in code.lines() {
        if let Some(x) = l.strip_prefix("# ") {
            result += x;
        } else {
            result += l;
        }
        result += "\n";
    }
    result
}

fn escape_html_char(c: char) -> String {
    match c {
        '<' => "&lt;",
        '>' => "&gt;",
        '&' => "&amp;",
        _ => return c.to_string(),
    }
    .to_string()
}

#[derive(Default)]
struct HoverStore {
    map: HashMap<String, usize>,
}

impl HoverStore {
    fn get_id(&mut self, hover: String) -> usize {
        if let Some(x) = self.map.get(&hover) {
            return *x;
        }
        let id = self.map.len();
        self.map.insert(hover, id);
        id
    }

    fn to_vec(self) -> Vec<String> {
        let mut r = vec!["".to_string(); self.map.len()];
        for (k, v) in self.map {
            r[v] = k;
        }
        r
    }
}

struct MyRA {
    host: AnalysisHost,
    file_id: FileId,
}

impl MyRA {
    fn setup(cargo_toml: Option<PathBuf>) -> Result<Self, Error> {
        let path_str = "/tmp/mdbook-ra/playcrate";
        let p = PathBuf::from(path_str);
        if p.exists() {
            fs::remove_dir_all(&p)?;
        }
        Command::new("cargo")
            .args(["init", "--bin", path_str])
            .spawn()?
            .wait()?;
        if let Some(p) = cargo_toml {
            fs::copy(p, "/tmp/mdbook-ra/playcrate/Cargo.toml").unwrap();
        }
        let no_progress = &|_| ();
        let load_config = LoadCargoConfig {
            load_out_dirs_from_check: true,
            with_proc_macro: true,
            prefill_caches: false,
        };
        let (host, vfs, _) = load_workspace_at(&p, &Default::default(), &load_config, no_progress)?;
        Ok(MyRA {
            host,
            file_id: vfs
                .file_id(&VfsPath::new_real_path(format!("{}/src/main.rs", path_str)))
                .unwrap(),
        })
    }

    fn analysis(&mut self, code: String) -> Analysis {
        let mut change = Change::new();
        change.change_file(self.file_id, Some(Arc::new(code)));
        self.host.apply_change(change);
        self.host.analysis()
    }
}

#[derive(Default)]
struct MyConfig {
    disabled_by_default: bool,
    cargo_toml: Option<PathBuf>,
}

impl Preprocessor for Nop {
    fn name(&self) -> &str {
        "ra"
    }

    fn run(&self, ctx: &PreprocessorContext, mut book: Book) -> Result<Book, Error> {
        let config = {
            let mut c = MyConfig::default();
            if let Some(m) = ctx.config.get_preprocessor(self.name()) {
                if m.get("disabled_by_default") == Some(&true.into()) {
                    c.disabled_by_default = true;
                }
                if let Some(Value::String(s)) = m.get("cargo_toml") {
                    c.cargo_toml = Some(PathBuf::from(s));
                }
            }
            c
        };

        let mut ra = MyRA::setup(config.cargo_toml)?;
        let disabled = config.disabled_by_default;
        book.for_each_mut(|book_item| {
            let chapter = if let mdbook::BookItem::Chapter(c) = book_item {
                c
            } else {
                eprintln!("{:#?}", book_item);
                return;
            };
            let mut hover_store = HoverStore::default();
            chapter.content = run_on_codes(&chapter.content, |original_code| {
                let (flags, code) = original_code.split_once("\n").unwrap();
                let main_added = &format!("# #![allow(unused)]\n# fn main() {{\n{}# }}", code);
                let code = if code.contains("fn main") {
                    code
                } else {
                    &main_added
                };
                eprintln!("{}", flags);
                if flags.contains("ra_disabled") || disabled && !flags.contains("ra_enabled") {
                    return original_code.to_string();
                }
                let mut result = String::new();
                let analysis = ra.analysis(raw_code(code).to_string());
                let file_id = ra.file_id;
                let static_index = StaticIndex::compute(&analysis);
                let file = static_index
                    .files
                    .into_iter()
                    .find(|x| x.file_id == file_id)
                    .unwrap();
                let mut additions: HashMap<usize, String> = Default::default();
                let mut add = |r: TextSize, s: String| {
                    *additions.entry(r.into()).or_insert("".to_string()) += &s;
                };
                for (r, id) in file.tokens {
                    let token = static_index.tokens.get(id).unwrap();
                    let hover_string = token
                        .hover
                        .as_ref()
                        .map(|x| &x.markup)
                        .map(|x| x.to_string());
                    let hover_string = if let Some(x) = hover_string {
                        x
                    } else {
                        continue;
                    };
                    let hover_id = hover_store.get_id(hover_string);
                    add(
                        r.start(),
                        format!(r#"<span class="ra" data-hover="{}">"#, hover_id),
                    );
                    add(r.end(), "</span>".to_string());
                }
                for hint in file.inlay_hints {
                    if matches!(hint.kind, InlayKind::TypeHint | InlayKind::ChainingHint) {
                        add(
                            hint.range.end(),
                            format!(
                                r#"<span class="inlay-hint">: {}</span>"#,
                                {
                                    let mut result = "".to_string();
                                    for c in hint.label.to_string().chars() {
                                        result += &escape_html_char(c);
                                    }
                                    result
                                }
                            ),
                        );
                    } else {
                        add(
                            hint.range.start(),
                            format!(
                                r#"<span class="inlay-hint">{}: </span>"#,
                                hint.label.to_string()
                            ),
                        );
                    }
                }
                let mut i = 0;
                for l in code.lines() {
                    if let Some(x) = l.strip_prefix("# ") {
                        i += x.len() + 1;
                        result += r#"<span class="boring">"#;
                        result += x;
                        result += "\n";
                        result += r#"</span>"#;
                        continue;
                    }
                    for c in l.chars() {
                        if    let Some(x) = additions.get(&i) {
                            result += x;
                        }
                        result += &escape_html_char(c);
                        i += 1;
                    }
                    result += "\n";
                    i += 1;
                }
                result
            });
            let mut json = "".to_string();
            for x in &hover_store.to_vec() {
                json += &format!("'{}',", markdown_to_html(x, &Default::default()).replace("'", "#$%").replace('\n', "\\n").replace("<pre", "<per").replace("<code", "<cide"));
            }
            chapter.content += &format!(
                r#"

<script src="https://unpkg.com/@popperjs/core@2.10.2/dist/umd/popper.min.js" integrity="sha384-7+zCNj/IqJ95wo16oMtfsKbZ9ccEh31eOz1HGyDuCQ6wgnyJNSYdrPa03rtR1zdB" crossorigin="anonymous"></script>
<script src="https://unpkg.com/tippy.js@6.3.2/dist/tippy-bundle.umd.min.js" integrity="sha384-vApKv6LkBdPwmt/fNiQrBCVCZvuniXpG0b5UZhVrGAq1zXdZRSsPcWjGdVxkZJtX" crossorigin="anonymous"></script>
<script>
    const hoverData = [{}].map((x)=>x.replaceAll('#$%', "'").replaceAll('<per', '<pre').replaceAll('<cide', '<code'));
    window.onload = () => {{
        console.log("hello");
        tippy('.ra', {{
            content: (x) => {{
                const div = document.createElement('div');
                div.innerHTML = hoverData[x.dataset.hover];
                div.className = 'hover-holder';
                div.querySelectorAll('code').forEach((y) => y.innerHTML = hljs.highlight('rust', y.innerText).value);
                return div;
            }},
            allowHTML: true,
            delay: [200, 0],
            interactive: true,
            maxWidth: '80vw',
            appendTo: document.querySelector('.content'),
        }});
    }};
</script>
                "#,
                json,
            );
        });

        Ok(book)
    }

    fn supports_renderer(&self, renderer: &str) -> bool {
        renderer != "not-supported"
    }
}
