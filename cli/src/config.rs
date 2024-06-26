// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::style_ser;
use color_eyre::Result;
use curl::easy::Easy;
use eyre::{eyre, Report, WrapErr};
use nu_ansi_term::{Color, Style};
use rink_core::output::fmt::FmtToken;
use rink_core::parsing::datetime;
use rink_core::Context;
use rink_core::{loader::gnu_units, CURRENCY_FILE, DATES_FILE, DEFAULT_FILE};
use serde_derive::{Deserialize, Serialize};
use std::env;
use std::ffi::OsString;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::{
    collections::HashMap,
    fs::{read_to_string, File},
};
use ubyte::ByteUnit;

fn file_to_string(mut file: File) -> Result<String> {
    let mut string = String::new();
    let _ = file.read_to_string(&mut string)?;
    Ok(string)
}

pub fn config_path(name: &'static str) -> Result<PathBuf> {
    let mut path = dirs::config_dir().ok_or_else(|| eyre!("Could not find config directory"))?;
    path.push("rink");
    path.push(name);
    Ok(path)
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub rink: Rink,
    pub currency: Currency,
    pub colors: Colors,
    pub limits: Limits,
    pub themes: HashMap<String, Theme>,
    // Hack because none of ansi-term's functionality is const safe.
    default_theme: Theme,
    disabled_theme: Theme,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Rink {
    /// Which prompt to render when run interactively.
    pub prompt: String,
    /// Use multi-line output for lists.
    pub long_output: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Currency {
    /// Set to false to disable currency loading entirely.
    pub enabled: bool,
    /// Set to false to only reuse the existing cached currency data.
    pub fetch_on_startup: bool,
    /// Which web endpoint should be used to download currency data?
    pub endpoint: String,
    /// How long to cache for.
    #[serde(with = "humantime_serde")]
    pub cache_duration: Duration,
    /// How long to wait before timing out requests.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    /// Whether support for colored output should be enabled.
    pub enabled: Option<bool>,
    /// The name of the current theme.
    pub theme: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Whether support for sandboxing is enabled.
    pub enabled: bool,
    /// Whether performance metrics are shown after each query.
    pub show_metrics: bool,
    /// The maximum amount of heap that can be used per query, in megabytes.
    pub memory: ByteUnit,
    /// How long to wait before cancelling a query.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    #[serde(with = "style_ser")]
    plain: Style,
    #[serde(with = "style_ser")]
    error: Style,
    #[serde(with = "style_ser")]
    unit: Style,
    #[serde(with = "style_ser")]
    quantity: Style,
    #[serde(with = "style_ser")]
    number: Style,
    #[serde(with = "style_ser")]
    user_input: Style,
    #[serde(with = "style_ser")]
    doc_string: Style,
    #[serde(with = "style_ser")]
    pow: Style,
    #[serde(with = "style_ser")]
    prop_name: Style,
    #[serde(with = "style_ser")]
    date_time: Style,
    #[serde(with = "style_ser")]
    link: Style,
}

impl Theme {
    pub fn get_style(&self, token: FmtToken) -> Style {
        match token {
            FmtToken::Plain => self.plain,
            FmtToken::Error => self.error,
            FmtToken::Unit => self.unit,
            FmtToken::Quantity => self.quantity,
            FmtToken::Number => self.number,
            FmtToken::UserInput => self.user_input,
            FmtToken::DocString => self.doc_string,
            FmtToken::Pow => self.pow,
            FmtToken::PropName => self.prop_name,
            FmtToken::DateTime => self.date_time,
            FmtToken::Link => self.link,

            // Default styling since these are handled specially.
            FmtToken::ListBegin => self.plain,
            FmtToken::ListSep => self.plain,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            rink: Default::default(),
            currency: Default::default(),
            colors: Default::default(),
            themes: Default::default(),
            limits: Default::default(),
            default_theme: Theme {
                plain: Style::default(),
                error: Style::new().fg(Color::Red),
                unit: Style::new().fg(Color::Cyan),
                quantity: Style::new().fg(Color::Cyan).dimmed(),
                number: Style::default(),
                user_input: Style::new().bold(),
                doc_string: Style::new().italic(),
                pow: Style::default(),
                prop_name: Style::new().fg(Color::Cyan),
                date_time: Style::default(),
                link: Style::new().fg(Color::Blue),
            },
            disabled_theme: Theme::default(),
        }
    }
}

impl Default for Currency {
    fn default() -> Self {
        Currency {
            enabled: true,
            fetch_on_startup: true,
            endpoint: "https://rinkcalc.app/data/currency.json".to_owned(),
            cache_duration: Duration::from_secs(60 * 60), // 1 hour
            timeout: Duration::from_secs(2),
        }
    }
}

impl Default for Rink {
    fn default() -> Self {
        Rink {
            prompt: "> ".to_owned(),
            long_output: false,
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            enabled: None,
            theme: "default".to_owned(),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            enabled: false,
            show_metrics: false,
            memory: ByteUnit::Megabyte(20),
            timeout: Duration::from_secs(10),
        }
    }
}

impl Config {
    pub fn get_theme(&self) -> &Theme {
        let default_enable_colors = env::var("NO_COLOR") == Err(env::VarError::NotPresent);
        let colors_enabled = self.colors.enabled.unwrap_or(default_enable_colors);

        if colors_enabled {
            let name = &self.colors.theme;
            let theme = self.themes.get(name);
            theme.unwrap_or(&self.default_theme)
        } else {
            &self.disabled_theme
        }
    }
}

fn read_from_search_path(
    filename: &str,
    paths: &[PathBuf],
    default: Option<&'static str>,
) -> Result<Vec<String>> {
    let result: Vec<String> = paths
        .iter()
        .filter_map(|path| {
            let mut buf = PathBuf::from(path);
            buf.push(filename);
            read_to_string(buf).ok()
        })
        .chain(default.map(ToOwned::to_owned))
        .collect();

    if result.is_empty() {
        Err(eyre!(
            "Could not find {}, and rink was not built with one bundled. Search path:{}",
            filename,
            paths
                .iter()
                .map(|path| format!("\n  {}", path.display()))
                .collect::<Vec<String>>()
                .join("")
        ))
    } else {
        Ok(result)
    }
}

pub(crate) fn force_refresh_currency(config: &Currency) -> Result<String> {
    println!("Fetching...");
    let start = std::time::Instant::now();
    let mut path = dirs::cache_dir().ok_or_else(|| eyre!("Could not find cache directory"))?;
    path.push("rink");
    path.push("currency.json");
    let file = download_to_file(&path, &config.endpoint, config.timeout)
        .wrap_err("Fetching currency data failed")?;
    let delta = std::time::Instant::now() - start;
    let metadata = file
        .metadata()
        .wrap_err("Fetched currency file, but failed to read file metadata")?;
    let len = metadata.len();
    Ok(format!(
        "Fetched {len} byte currency file after {}ms",
        delta.as_millis()
    ))
}

fn load_live_currency(config: &Currency) -> Result<String> {
    let duration = if config.fetch_on_startup {
        Some(config.cache_duration)
    } else {
        None
    };
    let file = cached("currency.json", &config.endpoint, duration, config.timeout)?;
    let contents = file_to_string(file)?;
    Ok(contents)
}

fn try_load_currency(config: &Currency, ctx: &mut Context, search_path: &[PathBuf]) -> Result<()> {
    let base = read_from_search_path("currency.units", search_path, CURRENCY_FILE)?
        .into_iter()
        .next()
        .unwrap();
    let live_defs = load_live_currency(config)?;
    ctx.load_currency(&live_defs, &base)
        .map_err(|err| eyre!("{err}"))?;
    Ok(())
}

pub fn read_config() -> Result<Config> {
    match read_to_string(config_path("config.toml")?) {
        // Hard fail if the file has invalid TOML.
        Ok(result) => toml::from_str(&result).wrap_err("While parsing config.toml"),
        // Use default config if it doesn't exist.
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Config::default()),
        // Hard fail for other IO errors (e.g. permissions).
        Err(err) => Err(eyre!(err).wrap_err("Failed to read config.toml")),
    }
}

/// Creates a context by searching standard directories
pub fn load(config: &Config) -> Result<Context> {
    let mut search_path = vec![PathBuf::from("./")];
    if let Some(mut config_dir) = dirs::config_dir() {
        config_dir.push("rink");
        search_path.push(config_dir);
    }
    if let Some(prefix) = option_env!("RINK_PATH") {
        search_path.push(prefix.into());
    }

    // Read definitions.units
    let units_files = read_from_search_path("definitions.units", &search_path, DEFAULT_FILE)?;

    let unit_defs: Vec<_> = units_files
        .iter()
        .map(|s| gnu_units::parse_str(s).defs)
        .flatten()
        .collect();

    // Read datepatterns.txt
    let dates = read_from_search_path("datepatterns.txt", &search_path, DATES_FILE)?
        .into_iter()
        .next()
        .unwrap();

    let mut ctx = Context::new();
    ctx.save_previous_result = true;
    ctx.load(rink_core::ast::Defs { defs: unit_defs })
        .map_err(|err| eyre!(err))?;
    ctx.load_dates(datetime::parse_datefile(&dates));

    // Load currency data.
    if config.currency.enabled {
        match try_load_currency(&config.currency, &mut ctx, &search_path) {
            Ok(()) => (),
            Err(err) => {
                println!("{:?}", err.wrap_err("Failed to load currency data"));
            }
        }
    }

    Ok(ctx)
}

fn read_if_current(file: File, expiration: Option<Duration>) -> Result<File> {
    use std::time::SystemTime;

    let stats = file.metadata()?;
    let mtime = stats.modified()?;
    let now = SystemTime::now();
    let elapsed = now.duration_since(mtime)?;
    if let Some(expiration) = expiration {
        if elapsed > expiration {
            return Err(eyre!("File is out of date"));
        }
    }
    Ok(file)
}

fn download_to_file(path: &Path, url: &str, timeout: Duration) -> Result<File> {
    use std::fs::create_dir_all;

    create_dir_all(path.parent().unwrap())?;

    // Given a filename like `foo.json`, names the temp file something
    // similar, like `foo.ABCDEF.json`.
    let mut prefix = path.file_stem().unwrap().to_owned();
    prefix.push(".");
    let mut suffix = OsString::from(".");
    suffix.push(path.extension().unwrap());

    // The temp file should be created in the same directory as its
    // final home, as the default temporary file directory might not be
    // the same filesystem.
    let mut temp_file = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(&suffix)
        .tempfile_in(path.parent().unwrap())?;

    let mut easy = Easy::new();
    easy.url(url)?;
    easy.useragent(&format!(
        "rink-cli {} <{}>",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    ))?;
    easy.timeout(timeout)?;

    let mut write_handle = temp_file.as_file_mut().try_clone()?;
    easy.write_function(move |data| {
        write_handle
            .write(data)
            .map_err(|_| curl::easy::WriteError::Pause)
    })?;
    easy.perform()?;

    let status = easy.response_code()?;
    if status != 200 {
        return Err(eyre!(
            "Received status {} while downloading {}",
            status,
            url
        ));
    }

    temp_file.as_file_mut().sync_all()?;
    temp_file.as_file_mut().seek(SeekFrom::Start(0))?;

    temp_file
        .persist(path)
        .wrap_err("Failed to write to cache dir")
}

fn cached(
    filename: &str,
    url: &str,
    expiration: Option<Duration>,
    timeout: Duration,
) -> Result<File> {
    let mut path = dirs::cache_dir().ok_or_else(|| eyre!("Could not find cache directory"))?;
    path.push("rink");
    path.push(filename);

    // 1. Return file if it exists and is up to date.
    // 2. Try to download a new version of the file and return it.
    // 3. If that fails, return the stale file if it exists.

    if let Ok(file) = File::open(&path) {
        if let Ok(result) = read_if_current(file, expiration) {
            return Ok(result);
        }
    }

    let err = match download_to_file(&path, url, timeout) {
        Ok(result) => return Ok(result),
        Err(err) => err,
    };

    if let Ok(file) = File::open(&path) {
        // Indicate error even though we're returning success.
        println!(
            "{:?}",
            Report::wrap_err(
                err,
                format!("Failed to refresh {}, using stale version", url)
            )
        );
        Ok(file)
    } else {
        Err(err).wrap_err_with(|| format!("Failed to fetch {}", url))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use once_cell::sync::Lazy;
    use tiny_http::{Response, Server, StatusCode};

    static SERVER: Lazy<Mutex<Arc<Server>>> = Lazy::new(|| {
        Mutex::new(Arc::new(
            Server::http("127.0.0.1:3090").expect("port 3090 is needed to do http tests"),
        ))
    });

    #[test]
    fn test_download_timeout() {
        let server = SERVER.lock().unwrap();
        let server2 = server.clone();

        let thread_handle = std::thread::spawn(move || {
            let request = server2.recv().expect("the request should not fail");
            assert_eq!(request.url(), "/data/currency.json");
            std::thread::sleep(Duration::from_millis(100));
        });
        let result = super::download_to_file(
            &PathBuf::from("currency.json"),
            "http://127.0.0.1:3090/data/currency.json",
            Duration::from_millis(5),
        );
        let result = result.expect_err("this should always fail");
        let result = result.to_string();
        assert!(result.starts_with("[28] Timeout was reached (Operation timed out after "));
        assert!(result.ends_with(" milliseconds with 0 bytes received)"));
        thread_handle.join().unwrap();
        drop(server);
    }

    #[test]
    fn test_download_404() {
        let server = SERVER.lock().unwrap();
        let server2 = server.clone();

        let thread_handle = std::thread::spawn(move || {
            let request = server2.recv().expect("the request should not fail");
            assert_eq!(request.url(), "/data/currency.json");
            let mut data = b"404 not found".to_owned();
            let cursor = std::io::Cursor::new(&mut data);
            request
                .respond(Response::new(StatusCode(404), vec![], cursor, None, None))
                .expect("the response should go through");
        });
        let result = super::download_to_file(
            &PathBuf::from("currency.json"),
            "http://127.0.0.1:3090/data/currency.json",
            Duration::from_millis(2000),
        );
        let result = result.expect_err("this should always fail");
        assert_eq!(
            result.to_string(),
            "Received status 404 while downloading http://127.0.0.1:3090/data/currency.json"
        );
        thread_handle.join().unwrap();
        drop(server);
    }

    #[test]
    fn test_download_success() {
        let server = SERVER.lock().unwrap();
        let server2 = server.clone();

        let thread_handle = std::thread::spawn(move || {
            let request = server2.recv().expect("the request should not fail");
            assert_eq!(request.url(), "/data/currency.json");
            let mut data = include_bytes!("../../core/tests/currency.snapshot.json").to_owned();
            let cursor = std::io::Cursor::new(&mut data);
            request
                .respond(Response::new(StatusCode(200), vec![], cursor, None, None))
                .expect("the response should go through");
        });
        let result = super::download_to_file(
            &PathBuf::from("currency.json"),
            "http://127.0.0.1:3090/data/currency.json",
            Duration::from_millis(2000),
        );
        let mut result = result.expect("this should succeed");
        let mut string = String::new();
        result
            .read_to_string(&mut string)
            .expect("the file should exist");
        assert_eq!(
            string,
            include_str!("../../core/tests/currency.snapshot.json")
        );
        thread_handle.join().unwrap();
        drop(server);
    }

    #[test]
    fn test_force_refresh_success() {
        let config = super::Currency {
            enabled: true,
            fetch_on_startup: false,
            endpoint: "http://127.0.0.1:3090/data/currency.json".to_owned(),
            cache_duration: Duration::ZERO,
            timeout: Duration::from_millis(2000),
        };

        let server = SERVER.lock().unwrap();
        let server2 = server.clone();

        let thread_handle = std::thread::spawn(move || {
            let request = server2.recv().expect("the request should not fail");
            assert_eq!(request.url(), "/data/currency.json");
            let mut data = include_bytes!("../../core/tests/currency.snapshot.json").to_owned();
            let cursor = std::io::Cursor::new(&mut data);
            request
                .respond(Response::new(StatusCode(200), vec![], cursor, None, None))
                .expect("the response should go through");
        });
        let result = super::force_refresh_currency(&config);
        let result = result.expect("this should succeed");
        assert!(result.starts_with("Fetched 6599 byte currency file after "));
        thread_handle.join().unwrap();
        drop(server);
    }

    #[test]
    fn test_force_refresh_timeout() {
        let config = super::Currency {
            enabled: true,
            fetch_on_startup: false,
            endpoint: "http://127.0.0.1:3090/data/currency.json".to_owned(),
            cache_duration: Duration::ZERO,
            timeout: Duration::from_millis(5),
        };

        let server = SERVER.lock().unwrap();
        let server2 = server.clone();

        let thread_handle = std::thread::spawn(move || {
            let request = server2.recv().expect("the request should not fail");
            assert_eq!(request.url(), "/data/currency.json");
            std::thread::sleep(Duration::from_millis(100));
        });
        let result = super::force_refresh_currency(&config);
        let result = result.expect_err("this should timeout");
        assert_eq!(result.to_string(), "Fetching currency data failed");
        thread_handle.join().unwrap();
        drop(server);
    }
}
