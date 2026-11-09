#![cfg_attr(windows, windows_subsystem = "windows")]
use anyhow::{bail, Context, Result};
use eframe::egui;
use reqwest::blocking::Client;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io};

const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

// ---------- shared state ----------
#[derive(Default)]
struct Shared {
    log: Vec<String>,
    busy: bool,
    versions: Vec<String>,
    hits: Vec<(String, String)>,
}
type S = Arc<Mutex<Shared>>;

fn say(sh: &S, m: impl Into<String>) {
    sh.lock().unwrap().log.push(m.into());
}
fn client() -> Client {
    Client::builder().user_agent("horizon-launcher/1.0").build().unwrap()
}
fn run(sh: &S, f: impl FnOnce(&S) -> Result<()> + Send + 'static) {
    let sh = sh.clone();
    sh.lock().unwrap().busy = true;
    std::thread::spawn(move || {
        if let Err(e) = f(&sh) {
            say(&sh, format!("Error: {e:#}"));
        }
        sh.lock().unwrap().busy = false;
    });
}

// ---------- helpers ----------
fn os_name() -> &'static str {
    if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "osx" } else { "linux" }
}
fn root() -> PathBuf {
    dirs::data_dir().expect("no data dir").join("horizon")
}
fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}
fn get(c: &Client, url: &str) -> Result<Value> {
    Ok(c.get(url).send()?.error_for_status()?.json()?)
}
fn sha1_hex(d: &[u8]) -> String {
    format!("{:x}", Sha1::digest(d))
}
fn allowed(rules: Option<&Value>) -> bool {
    let Some(rules) = rules.and_then(Value::as_array) else { return true };
    let mut ok = false;
    for r in rules {
        let os_ok = r.get("os").and_then(|o| o.get("name")).and_then(Value::as_str).map_or(true, |n| n == os_name());
        if os_ok && r.get("features").is_none() {
            ok = r["action"] == "allow";
        }
    }
    ok
}
fn fetch(c: &Client, url: &str, dest: &Path, sha1: Option<&str>) -> Result<()> {
    if let Ok(old) = fs::read(dest) {
        if sha1.map_or(true, |h| sha1_hex(&old) == h) {
            return Ok(());
        }
    }
    if url.is_empty() {
        bail!("missing file {}", dest.display());
    }
    fs::create_dir_all(dest.parent().unwrap())?;
    let bytes = c.get(url).send()?.error_for_status()?.bytes()?;
    if let Some(h) = sha1 {
        if sha1_hex(&bytes) != h {
            bail!("checksum mismatch for {url}");
        }
    }
    fs::write(dest, &bytes)?;
    Ok(())
}

// ---------- version json (with inheritsFrom merging) ----------
fn ga(n: &str) -> String {
    let p: Vec<&str> = n.split(':').collect();
    format!("{}:{}:{}", p[0], p.get(1).unwrap_or(&""), p.get(3).unwrap_or(&""))
}

fn load_version(r: &Path, id: &str) -> Result<Value> {
    let raw = fs::read(r.join("versions").join(id).join(format!("{id}.json"))).with_context(|| format!("{id} not installed"))?;
    let mut v: Value = serde_json::from_slice(&raw)?;
    v["_jar"] = id.into();
    let Some(p) = v["inheritsFrom"].as_str().map(String::from) else { return Ok(v) };
    let mut m = load_version(r, &p)?;
    let mut libs = v["libraries"].as_array().cloned().unwrap_or_default();
    libs.extend(m["libraries"].as_array().cloned().unwrap_or_default());
    let mut seen = HashSet::new();
    libs.retain(|l| seen.insert(ga(s(l, "name"))));
    m["libraries"] = Value::Array(libs);
    for k in ["mainClass", "minecraftArguments"] {
        if !v[k].is_null() {
            m[k] = v[k].clone();
        }
    }
    for k in ["jvm", "game"] {
        if let Some(extra) = v["arguments"][k].as_array() {
            let mut a = m["arguments"][k].as_array().cloned().unwrap_or_default();
            a.extend(extra.clone());
            m["arguments"][k] = Value::Array(a);
        }
    }
    m["id"] = v["id"].clone();
    Ok(m)
}

/// (relative path, url, sha1) for a library entry, handling plain Maven-style entries too.
fn lib_file(l: &Value) -> Option<(String, String, Option<String>)> {
    if let Some(a) = l["downloads"].get("artifact") {
        return Some((s(a, "path").into(), s(a, "url").into(), a["sha1"].as_str().map(String::from)));
    }
    if l["downloads"].is_null() && l["name"].is_string() {
        let base = l["url"].as_str().unwrap_or("https://libraries.minecraft.net/");
        let p: Vec<&str> = s(l, "name").split(':').collect();
        let cls = p.get(3).map_or(String::new(), |c| format!("-{c}"));
        let path = format!("{}/{}/{}/{}-{}{cls}.jar", p[0].replace('.', "/"), p[1], p[2], p[1], p[2]);
        return Some((path.clone(), format!("{base}{path}"), None));
    }
    None
}

// ---------- installers ----------
fn load_versions(sh: &S) -> Result<()> {
    let m = get(&client(), MANIFEST)?;
    let mut out = vec![];
    for v in m["versions"].as_array().unwrap() {
        if s(v, "type") == "release" {
            out.push(s(v, "id").to_string());
            if s(v, "id") == "1.8.9" {
                break;
            }
        }
    }
    sh.lock().unwrap().versions = out;
    Ok(())
}

fn install_vanilla(c: &Client, r: &Path, id: &str) -> Result<()> {
    let m = get(c, MANIFEST)?;
    let e = m["versions"].as_array().unwrap().iter().find(|v| v["id"] == id).with_context(|| format!("unknown version {id}"))?;
    let vdir = r.join("versions").join(id);
    fetch(c, s(e, "url"), &vdir.join(format!("{id}.json")), e["sha1"].as_str())?;
    let v = load_version(r, id)?;
    let cl = &v["downloads"]["client"];
    fetch(c, s(cl, "url"), &vdir.join(format!("{id}.jar")), cl["sha1"].as_str())
}

fn install_fabric(c: &Client, r: &Path, mc: &str) -> Result<String> {
    let l = get(c, &format!("https://meta.fabricmc.net/v2/versions/loader/{mc}"))?;
    let lv = l.as_array().and_then(|a| a.iter().find(|x| x["loader"]["stable"] == true))
        .map(|x| s(&x["loader"], "version").to_string())
        .with_context(|| format!("Fabric doesn't support {mc} (it starts at 1.14)"))?;
    let p = get(c, &format!("https://meta.fabricmc.net/v2/versions/loader/{mc}/{lv}/profile/json"))?;
    let id = s(&p, "id").to_string();
    let f = r.join("versions").join(&id).join(format!("{id}.json"));
    fs::create_dir_all(f.parent().unwrap())?;
    fs::write(f, serde_json::to_vec(&p)?)?;
    Ok(id)
}

fn list_versions(r: &Path) -> Vec<String> {
    fs::read_dir(r.join("versions")).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).collect()
}

fn install_forge(c: &Client, r: &Path, mc: &str, java: &Path, sh: &S) -> Result<String> {
    let pr = get(c, "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json")?;
    let fv = pr["promos"][format!("{mc}-recommended")].as_str().or(pr["promos"][format!("{mc}-latest")].as_str())
        .with_context(|| format!("No Forge build exists for {mc}"))?.to_string();
    let find = || list_versions(r).into_iter().find(|v| v.contains("forge") && v.contains(&fv));
    if let Some(id) = find() {
        return Ok(id);
    }
    say(sh, format!("Running Forge {fv} installer (this takes a minute)..."));
    let full = if mc == "1.8.9" { format!("{mc}-{fv}-{mc}") } else { format!("{mc}-{fv}") };
    let jar = r.join("cache").join(format!("forge-{full}-installer.jar"));
    fetch(c, &format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{full}/forge-{full}-installer.jar"), &jar, None)?;
    let lp = r.join("launcher_profiles.json");
    if !lp.exists() {
        fs::write(&lp, r#"{"profiles":{}}"#)?;
    }
    let st = Command::new(java).arg("-jar").arg(&jar).arg("--installClient").arg(r).current_dir(r).status()?;
    if !st.success() {
        bail!("Forge installer failed");
    }
    find().context("Forge installed but its version folder wasn't found")
}

fn extract_natives(jar: &Path, out: &Path) -> Result<()> {
    let mut z = zip::ZipArchive::new(fs::File::open(jar)?)?;
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let n = f.name().to_string();
        if [".dll", ".so", ".dylib", ".jnilib"].iter().any(|e| n.ends_with(e)) {
            io::copy(&mut f, &mut fs::File::create(out.join(Path::new(&n).file_name().unwrap()))?)?;
        }
    }
    Ok(())
}

fn ensure_deps(c: &Client, r: &Path, id: &str, sh: &S) -> Result<()> {
    let v = load_version(r, id)?;
    let nat = r.join("versions").join(id).join("natives");
    fs::create_dir_all(&nat)?;
    let libs: Vec<&Value> = v["libraries"].as_array().into_iter().flatten().filter(|l| allowed(l.get("rules"))).collect();
    say(sh, format!("Checking {} libraries...", libs.len()));
    for l in libs {
        if let Some((p, u, h)) = lib_file(l) {
            fetch(c, &u, &r.join("libraries").join(p), h.as_deref())?;
        }
        if let Some(k) = l["natives"][os_name()].as_str() {
            if let Some(a) = l["downloads"]["classifiers"].get(k.replace("${arch}", "64")) {
                let p = r.join("libraries").join(s(a, "path"));
                fetch(c, s(a, "url"), &p, a["sha1"].as_str())?;
                extract_natives(&p, &nat)?;
            }
        }
    }
    say(sh, "Checking assets...");
    let ai = &v["assetIndex"];
    let idx = r.join("assets/indexes").join(format!("{}.json", s(ai, "id")));
    fetch(c, s(ai, "url"), &idx, ai["sha1"].as_str())?;
    let index: Value = serde_json::from_slice(&fs::read(&idx)?)?;
    let hashes: Vec<String> = index["objects"].as_object().unwrap().values().map(|o| s(o, "hash").to_string()).collect();
    let failed = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for chunk in hashes.chunks(hashes.len() / 16 + 1) {
            let failed = &failed;
            sc.spawn(move || {
                for h in chunk {
                    let p = &h[..2];
                    let url = format!("https://resources.download.minecraft.net/{p}/{h}");
                    if fetch(c, &url, &r.join("assets/objects").join(p).join(h), Some(h)).is_err() {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
    let n = failed.load(Ordering::Relaxed);
    if n > 0 {
        bail!("{n} assets failed to download; press Play again to retry");
    }
    Ok(())
}

// ---------- Java (auto-downloaded, no setup) ----------
fn find_java(d: &Path) -> Option<PathBuf> {
    for e in fs::read_dir(d).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(j) = find_java(&p) {
                return Some(j);
            }
        } else if p.file_name()?.to_str() == Some(if cfg!(windows) { "java.exe" } else { "java" }) && p.parent()?.ends_with("bin") {
            return Some(p);
        }
    }
    None
}

fn java_for(c: &Client, r: &Path, major: u64, sh: &S) -> Result<PathBuf> {
    let d = r.join("runtime").join(major.to_string());
    if let Some(j) = find_java(&d) {
        return Ok(j);
    }
    say(sh, format!("Downloading Java {major} (one time only)..."));
    let os = if os_name() == "osx" { "mac" } else { os_name() };
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x64" };
    let url = format!("https://api.adoptium.net/v3/binary/latest/{major}/ga/{os}/{arch}/jre/hotspot/normal/eclipse");
    let bytes = c.get(url).send()?.error_for_status()?.bytes()?;
    fs::create_dir_all(&d)?;
    if cfg!(windows) {
        zip::ZipArchive::new(io::Cursor::new(bytes))?.extract(&d)?;
    } else {
        tar::Archive::new(flate2::read::GzDecoder::new(io::Cursor::new(bytes))).unpack(&d)?;
    }
    find_java(&d).context("Java was downloaded but couldn't be found")
}

// ---------- launch ----------
fn offline_uuid(name: &str) -> String {
    let mut b = md5::compute(format!("OfflinePlayer:{name}")).0;
    b[6] = (b[6] & 0x0f) | 0x30;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn collect_args(arr: &Value) -> Vec<String> {
    let mut out = vec![];
    for a in arr.as_array().into_iter().flatten() {
        match a {
            Value::String(x) => out.push(x.clone()),
            Value::Object(_) if allowed(a.get("rules")) => match &a["value"] {
                Value::String(x) => out.push(x.clone()),
                Value::Array(xs) => out.extend(xs.iter().filter_map(|x| x.as_str().map(String::from))),
                _ => {}
            },
            _ => {}
        }
    }
    out
}

fn launch(r: &Path, id: &str, user: &str, java: &Path, gd: &Path) -> Result<()> {
    let v = load_version(r, id)?;
    let jar = s(&v, "_jar");
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut cp: Vec<String> = v["libraries"].as_array().into_iter().flatten()
        .filter(|l| allowed(l.get("rules")))
        .filter_map(lib_file)
        .map(|(p, _, _)| r.join("libraries").join(p).to_string_lossy().into_owned())
        .collect();
    cp.push(r.join("versions").join(jar).join(format!("{jar}.jar")).to_string_lossy().into_owned());
    fs::create_dir_all(gd.join("mods"))?;
    let p = |x: &Path| x.to_string_lossy().into_owned();
    let vars: HashMap<&str, String> = HashMap::from([
        ("auth_player_name", user.to_string()),
        ("auth_uuid", offline_uuid(user)),
        ("auth_access_token", "0".into()),
        ("auth_xuid", "0".into()),
        ("clientid", "0".into()),
        ("user_type", "legacy".into()),
        ("user_properties", "{}".into()),
        ("version_name", id.into()),
        ("version_type", s(&v, "type").into()),
        ("game_directory", p(gd)),
        ("assets_root", p(&r.join("assets"))),
        ("assets_index_name", s(&v["assetIndex"], "id").into()),
        ("natives_directory", p(&r.join("versions").join(id).join("natives"))),
        ("library_directory", p(&r.join("libraries"))),
        ("classpath_separator", sep.into()),
        ("launcher_name", "horizon".into()),
        ("launcher_version", "1.0".into()),
        ("classpath", cp.join(sep)),
    ]);
    let sub = |a: &String| {
        let mut o = a.clone();
        for (k, val) in &vars {
            o = o.replace(&format!("${{{k}}}"), val);
        }
        o
    };
    let jvm = if v["arguments"]["jvm"].is_array() {
        collect_args(&v["arguments"]["jvm"])
    } else {
        vec!["-Djava.library.path=${natives_directory}".into(), "-cp".into(), "${classpath}".into()]
    };
    let game = if v["arguments"]["game"].is_array() {
        collect_args(&v["arguments"]["game"])
    } else {
        s(&v, "minecraftArguments").split_whitespace().map(String::from).collect()
    };
    Command::new(java)
        .arg("-Xmx3G")
        .args(jvm.iter().map(&sub))
        .arg(s(&v, "mainClass"))
        .args(game.iter().map(&sub))
        .current_dir(gd)
        .spawn()
        .context("couldn't start Java")?;
    Ok(())
}

// ---------- Modrinth ----------
fn search(q: &str, loader: &str, mc: &str) -> Result<Vec<(String, String)>> {
    let f = format!("[[\"project_type:mod\"],[\"categories:{loader}\"],[\"versions:{mc}\"]]");
    let r: Value = client().get("https://api.modrinth.com/v2/search")
        .query(&[("query", q), ("facets", f.as_str()), ("limit", "25")]).send()?.error_for_status()?.json()?;
    Ok(r["hits"].as_array().into_iter().flatten()
        .map(|h| (s(h, "project_id").into(), format!("{} - {}", s(h, "title"), s(h, "description")))).collect())
}

fn mod_install(c: &Client, pid: &str, loader: &str, mc: &str, dir: &Path, seen: &mut HashSet<String>, sh: &S) -> Result<()> {
    if !seen.insert(pid.into()) {
        return Ok(());
    }
    let vs: Value = c.get(format!("https://api.modrinth.com/v2/project/{pid}/version"))
        .query(&[("loaders", format!("[\"{loader}\"]")), ("game_versions", format!("[\"{mc}\"]"))])
        .send()?.error_for_status()?.json()?;
    let Some(v) = vs.as_array().and_then(|a| a.first()) else {
        say(sh, format!("No {loader} build of {pid} for {mc}"));
        return Ok(());
    };
    let f = v["files"].as_array().and_then(|a| a.iter().find(|f| f["primary"] == true).or(a.first())).context("mod has no files")?;
    fetch(c, s(f, "url"), &dir.join(s(f, "filename")), f["hashes"]["sha1"].as_str())?;
    say(sh, format!("Installed {}", s(f, "filename")));
    for d in v["dependencies"].as_array().into_iter().flatten() {
        if s(d, "dependency_type") == "required" {
            if let Some(p) = d["project_id"].as_str() {
                mod_install(c, p, loader, mc, dir, seen, sh)?;
            }
        }
    }
    Ok(())
}

// ---------- GUI ----------
#[derive(PartialEq, Clone, Copy)]
enum Loader { Vanilla, Fabric, Forge }
impl Loader {
    fn name(self) -> &'static str {
        match self { Loader::Vanilla => "vanilla", Loader::Fabric => "fabric", Loader::Forge => "forge" }
    }
}
fn inst(l: Loader, mc: &str) -> PathBuf {
    root().join("instances").join(format!("{}-{mc}", l.name()))
}

struct App { sh: S, user: String, loader: Loader, ver: String, q: String }

impl App {
    fn play(&self) {
        let (user, loader, mc) = (self.user.trim().to_string(), self.loader, self.ver.clone());
        run(&self.sh, move |sh| {
            let (c, r) = (client(), root());
            say(sh, format!("Preparing {} {mc}...", loader.name()));
            install_vanilla(&c, &r, &mc)?;
            let major = load_version(&r, &mc)?["javaVersion"]["majorVersion"].as_u64().unwrap_or(8);
            let java = java_for(&c, &r, major, sh)?;
            let id = match loader {
                Loader::Vanilla => mc.clone(),
                Loader::Fabric => install_fabric(&c, &r, &mc)?,
                Loader::Forge => install_forge(&c, &r, &mc, &java, sh)?,
            };
            ensure_deps(&c, &r, &id, sh)?;
            let user = if user.is_empty() { "Player".to_string() } else { user };
            launch(&r, &id, &user, &java, &inst(loader, &mc))?;
            say(sh, "Game started. Have fun!");
            Ok(())
        });
    }
    fn do_search(&self) {
        let (q, l, mc) = (self.q.clone(), self.loader.name(), self.ver.clone());
        run(&self.sh, move |sh| {
            let hits = search(&q, l, &mc)?;
            say(sh, format!("{} mods found", hits.len()));
            sh.lock().unwrap().hits = hits;
            Ok(())
        });
    }
    fn add_mod(&self, pid: String) {
        let (l, mc) = (self.loader, self.ver.clone());
        run(&self.sh, move |sh| {
            let dir = inst(l, &mc).join("mods");
            fs::create_dir_all(&dir)?;
            mod_install(&client(), &pid, l.name(), &mc, &dir, &mut HashSet::new(), sh)
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        let (versions, busy, hits, log) = {
            let g = self.sh.lock().unwrap();
            (g.versions.clone(), g.busy, g.hits.clone(), g.log.clone())
        };
        if self.ver.is_empty() {
            if let Some(v) = versions.first() {
                self.ver = v.clone();
            }
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Horizon Launcher");
            ui.horizontal(|ui| {
                ui.label("Username");
                ui.text_edit_singleline(&mut self.user);
                egui::ComboBox::from_id_source("loader").selected_text(self.loader.name()).show_ui(ui, |ui| {
                    for l in [Loader::Vanilla, Loader::Fabric, Loader::Forge] {
                        ui.selectable_value(&mut self.loader, l, l.name());
                    }
                });
                egui::ComboBox::from_id_source("ver").selected_text(self.ver.as_str()).show_ui(ui, |ui| {
                    for v in &versions {
                        ui.selectable_value(&mut self.ver, v.clone(), v.as_str());
                    }
                });
            });
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy && !self.ver.is_empty(), egui::Button::new("Install & Play")).clicked() {
                    self.play();
                }
                if ui.button("Open mods folder").clicked() {
                    let d = inst(self.loader, &self.ver).join("mods");
                    let _ = fs::create_dir_all(&d);
                    let _ = open::that(d);
                }
                if ui.button("OptiFine (official site)").clicked() {
                    let _ = open::that("https://optifine.net/downloads");
                }
            });
            ui.separator();
            ui.label("Modrinth mods (matches your loader + version)");
            ui.add_enabled_ui(self.loader != Loader::Vanilla && !busy, |ui| {
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.q);
                    if ui.button("Search").clicked() {
                        self.do_search();
                    }
                });
                egui::ScrollArea::vertical().id_source("hits").max_height(200.0).show(ui, |ui| {
                    for (id, t) in &hits {
                        ui.horizontal(|ui| {
                            if ui.button("Install").clicked() {
                                self.add_mod(id.clone());
                            }
                            ui.label(t);
                        });
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical().id_source("log").stick_to_bottom(true).show(ui, |ui| {
                for l in &log {
                    ui.monospace(l);
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let sh: S = Default::default();
    {
        let sh = sh.clone();
        std::thread::spawn(move || {
            if let Err(e) = load_versions(&sh) {
                say(&sh, format!("Couldn't load versions (offline?): {e:#}"));
            }
        });
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([780.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Horizon Launcher",
        opts,
        Box::new(move |_| Box::new(App { sh, user: "Player".into(), loader: Loader::Fabric, ver: String::new(), q: String::new() })),
    )
}
