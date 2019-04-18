use crate::flexi_error::FlexiLoggerError;
use crate::LevelFilter;

#[cfg(feature = "specfile")]
use log::error;
use regex::Regex;
#[cfg(feature = "specfile")]
use serde_derive::Deserialize;
#[cfg(feature = "specfile")]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
#[cfg(feature = "specfile")]
use std::ffi::OsStr;
#[cfg(feature = "specfile")]
use std::fs;
#[cfg(feature = "specfile")]
use std::io::Read;
use std::io::Write;
#[cfg(feature = "specfile")]
use std::path::{Path, PathBuf};
#[cfg(feature = "specfile")]
use toml;

///
/// Immutable struct that defines which loglines are to be written,
/// based on the module, the log level, and the text.
///
/// The loglevel specification via string (relevant for methods
/// [parse()](struct.LogSpecification.html#method.parse) and
/// [env()](struct.LogSpecification.html#method.env))
/// works essentially like with `env_logger`,
/// but we are a bit more tolerant with spaces. Its functionality can be
/// described with some Backus-Naur-form:
///
/// ```text
/// <log_level_spec> ::= single_log_level_spec[{,single_log_level_spec}][/<text_filter>]
/// <single_log_level_spec> ::= <path_to_module>|<log_level>|<path_to_module>=<log_level>
/// <text_filter> ::= <regex>
/// ```
///
/// * Examples:
///
///   * `"info"`: all logs with info, warn, or error level are written
///   * `"crate1"`: all logs of this crate are written, but nothing else
///   * `"warn, crate2::mod_a=debug, mod_x::mod_y=trace"`: all crates log warnings and errors,
///     `mod_a` additionally debug messages, and `mod_x::mod_y` is fully traced
///
/// * If you just specify the module, without `log_level`, all levels will be traced for this
///   module.
/// * If you just specify a log level, this will be applied as default to all modules without
///   explicit log level assigment.
///   (You see that for modules named error, warn, info, debug or trace,
///   it is necessary to specify their loglevel explicitly).
/// * The module names are compared as Strings, with the side effect that a specified module filter
///   affects all modules whose name starts with this String.<br>
///   Example: ```"foo"``` affects e.g.
///
///   * `foo`
///   * `foo::bar`
///   * `foobaz` (!)
///   * `foobaz::bar` (!)
///
/// The optional text filter is applied for all modules.
///
/// Note that external module names are to be specified like in ```"extern crate ..."```, i.e.,
/// for crates with a dash in their name this means: the dash is to be replaced with
/// the underscore (e.g. ```karl_heinz```, not ```karl-heinz```).
#[derive(Clone, Debug, Default)]
pub struct LogSpecification {
    module_filters: Vec<ModuleFilter>,
    textfilter: Option<Regex>,
}

/// Defines which loglevel filter to use for a given module (or as default, if no module is given).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleFilter {
    pub module_name: Option<String>,
    pub level_filter: LevelFilter,
}

impl LogSpecification {
    pub(crate) fn reconfigure(&mut self, other_spec: LogSpecification) {
        self.module_filters = other_spec.module_filters;
        self.textfilter = other_spec.textfilter;
        log::set_max_level(self.max_level());
    }

    pub(crate) fn max_level(&self) -> log::LevelFilter {
        self.module_filters
            .iter()
            .map(|d| d.level_filter)
            .max()
            .unwrap_or(log::LevelFilter::Off)
    }

    /// Implementation of Log::enabled() with easier testable signature
    pub fn enabled(&self, level: log::Level, target_module: &str) -> bool {
        // Search for the longest match, the vector is assumed to be pre-sorted.
        for module_filter in &self.module_filters {
            match module_filter.module_name {
                Some(ref module_name) if !target_module.starts_with(&**module_name) => {}
                Some(..) | None => return level <= module_filter.level_filter,
            }
        }
        false
    }

    /// Returns a `LogSpecification` where all traces are switched off.
    pub fn off() -> LogSpecification {
        Default::default()
    }

    /// Returns a log specification from a String.
    pub fn parse(spec: &str) -> Result<LogSpecification, FlexiLoggerError> {
        let mut parse_errs = Vec::<String>::new();
        let mut dirs = Vec::<ModuleFilter>::new();

        let mut parts = spec.split('/');
        let mods = parts.next();
        let filter = parts.next();
        if parts.next().is_some() {
            push_err(
                format!("invalid log spec '{}' (too many '/'s), ignoring it", spec),
                &mut parse_errs,
            );
            return parse_err(parse_errs, LogSpecification::off());
        }
        if let Some(m) = mods {
            for s in m.split(',') {
                let s = s.trim();
                if s.is_empty() {
                    continue;
                }
                let mut parts = s.split('=');
                let (log_level, name) = match (
                    parts.next().map(|s| s.trim()),
                    parts.next().map(|s| s.trim()),
                    parts.next(),
                ) {
                    (Some(part0), None, None) => {
                        if contains_dash_or_whitespace(part0, &mut parse_errs) {
                            continue;
                        }
                        // if the single argument is a log-level string or number,
                        // treat that as a global fallback setting
                        match parse_level_filter(part0.trim()) {
                            Ok(num) => (num, None),
                            Err(_) => (LevelFilter::max(), Some(part0)),
                        }
                    }

                    (Some(part0), Some(""), None) => {
                        if contains_dash_or_whitespace(part0, &mut parse_errs) {
                            continue;
                        }
                        (LevelFilter::max(), Some(part0))
                    }

                    (Some(part0), Some(part1), None) => {
                        if contains_dash_or_whitespace(part0, &mut parse_errs) {
                            continue;
                        }
                        match parse_level_filter(part1.trim()) {
                            Ok(num) => (num, Some(part0.trim())),
                            Err(e) => {
                                push_err(e.to_string(), &mut parse_errs);
                                continue;
                            }
                        }
                    }
                    _ => {
                        push_err(
                            format!("invalid part in log spec '{}', ignoring it", s),
                            &mut parse_errs,
                        );
                        continue;
                    }
                };
                dirs.push(ModuleFilter {
                    module_name: name.map(|s| s.to_string()),
                    level_filter: log_level,
                });
            }
        }

        let textfilter = filter.and_then(|filter| match Regex::new(filter) {
            Ok(re) => Some(re),
            Err(e) => {
                push_err(format!("invalid regex filter - {}", e), &mut parse_errs);
                None
            }
        });

        let logspec = LogSpecification {
            module_filters: dirs.level_sort(),
            textfilter,
        };

        if parse_errs.is_empty() {
            Ok(logspec)
        } else {
            Err(FlexiLoggerError::Parse(parse_errs, logspec))
        }
    }

    /// Returns a log specification based on the value of the environment variable RUST_LOG,
    /// or an empty one.
    pub fn env() -> Result<LogSpecification, FlexiLoggerError> {
        match env::var("RUST_LOG") {
            Ok(spec) => LogSpecification::parse(&spec),
            Err(..) => Ok(LogSpecification::off()),
        }
    }

    /// Returns a log specification based on the value of the environment variable RUST_LOG,
    /// or on the given String.
    pub fn env_or_parse<S: AsRef<str>>(
        given_spec: S,
    ) -> Result<LogSpecification, FlexiLoggerError> {
        match env::var("RUST_LOG") {
            Ok(spec) => LogSpecification::parse(&spec),
            Err(..) => LogSpecification::parse(given_spec.as_ref()),
        }
    }

    /// If the specfile does not exist, try to create it, with the current spec as content,
    /// under the specified name.
    #[cfg(feature = "specfile")]
    pub fn ensure_specfile_is_valid(&self, specfile: &PathBuf) -> Result<(), FlexiLoggerError> {
        if specfile
            .extension()
            .unwrap_or_else(|| OsStr::new(""))
            .to_str()
            .unwrap_or("")
            != "toml"
        {
            return Err(FlexiLoggerError::Parse(
                vec!["only files with suffix toml are supported".to_owned()],
                LogSpecification::off(),
            ));
        }

        if Path::is_file(specfile) {
            return Ok(());
        }

        if let Some(specfolder) = specfile.parent() {
            if let Err(e) = fs::DirBuilder::new().recursive(true).create(specfolder) {
                error!(
                    "cannot create the folder for the logspec file under the specified name \
                     {:?}, caused by: {}",
                    &specfile, e
                );
                return Err(FlexiLoggerError::from(e));
            }
        }

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(specfile)
        {
            Err(e) => {
                error!(
                    "cannot create an initial logspec file under the specified name \
                     {:?}, caused by: {}",
                    &specfile, e
                );
                return Err(FlexiLoggerError::from(e));
            }
            Ok(mut file) => {
                self.to_toml(&mut file)?;
            }
        };

        Ok(())
    }

    /// Reads a log specification from a file.
    #[cfg(feature = "specfile")]
    pub fn file<P: AsRef<Path>>(specfile: P) -> Result<LogSpecification, FlexiLoggerError> {
        // Open the file in read-only mode.
        let mut file = fs::File::open(specfile)?;

        // Read the content toml file as an instance of `LogSpecFileFormat`.
        let mut s = String::new();
        file.read_to_string(&mut s)?;
        LogSpecification::from_toml(&s)
    }

    //
    #[cfg(feature = "specfile")]
    fn from_toml(s: &str) -> Result<LogSpecification, FlexiLoggerError> {
        #[derive(Clone, Debug, Deserialize)]
        struct LogSpecFileFormat {
            pub global_level: Option<String>,
            pub global_pattern: Option<String>,
            pub modules: BTreeMap<String, String>,
        }

        let logspec_ff: LogSpecFileFormat = toml::from_str(s)?;
        let mut parse_errs = Vec::<String>::new();
        let mut module_filters = Vec::<ModuleFilter>::new();

        if let Some(s) = logspec_ff.global_level {
            module_filters.push(ModuleFilter {
                module_name: None,
                level_filter: parse_level_filter(s)?,
            });
        }

        for (k, v) in logspec_ff.modules {
            module_filters.push(ModuleFilter {
                module_name: Some(k),
                level_filter: parse_level_filter(v)?,
            });
        }

        let textfilter = match logspec_ff.global_pattern {
            None => None,
            Some(s) => match Regex::new(&s) {
                Ok(re) => Some(re),
                Err(e) => {
                    push_err(format!("invalid regex filter - {}", e), &mut parse_errs);
                    None
                }
            },
        };

        let logspec = LogSpecification {
            module_filters: module_filters.level_sort(),
            textfilter,
        };
        if parse_errs.is_empty() {
            Ok(logspec)
        } else {
            Err(FlexiLoggerError::Parse(parse_errs, logspec))
        }
    }

    /// Serializes itself in toml format
    pub fn to_toml(&self, w: &mut Write) -> Result<(), FlexiLoggerError> {
        w.write_all(b"### Optional: Default log level\n")?;
        let last = self.module_filters.last();
        if last.is_some() && last.as_ref().unwrap().module_name.is_none() {
            w.write_all(
                format!(
                    "global_level = '{}'\n",
                    last.as_ref()
                        .unwrap()
                        .level_filter
                        .to_string()
                        .to_lowercase()
                )
                .as_bytes(),
            )?;
        } else {
            w.write_all(b"#global_level = 'info'\n")?;
        }

        w.write_all(
            b"\n### Optional: specify a regular expression to suppress all messages that don't match\n",
        )?;
        w.write_all(b"#global_pattern = 'foo'\n")?;

        w.write_all(
            b"\n### Specific log levels per module are optionally defined in this section\n",
        )?;
        w.write_all(b"[modules]\n")?;
        if self.module_filters.is_empty() || self.module_filters[0].module_name.is_none() {
            w.write_all(b"#'mod1' = 'warn'\n")?;
            w.write_all(b"#'mod2' = 'debug'\n")?;
            w.write_all(b"#'mod2::mod3' = 'trace'\n")?;
        }
        for mf in &self.module_filters {
            if mf.module_name.is_some() {
                w.write_all(
                    format!(
                        "'{}' = '{}'\n",
                        mf.module_name.as_ref().unwrap(),
                        mf.level_filter.to_string().to_lowercase()
                    )
                    .as_bytes(),
                )?;
            }
        }
        Ok(())
    }

    /// Creates a LogSpecBuilder, setting the default log level.
    pub fn default(level_filter: LevelFilter) -> LogSpecBuilder {
        LogSpecBuilder::from_module_filters(&[ModuleFilter {
            module_name: None,
            level_filter,
        }])
    }

    /// Provides a reference to the module filters.
    pub fn module_filters(&self) -> &Vec<ModuleFilter> {
        &self.module_filters
    }

    /// Provides a reference to the text filter.
    pub fn text_filter(&self) -> &Option<Regex> {
        &(self.textfilter)
    }
}

fn push_err(s: String, parse_errs: &mut Vec<String>) {
    println!("flexi_logger warning: {}", s);
    parse_errs.push(s);
}

fn parse_err(
    errors: Vec<String>,
    logspec: LogSpecification,
) -> Result<LogSpecification, FlexiLoggerError> {
    Err(FlexiLoggerError::Parse(errors, logspec))
}

// #[cfg(feature = "specfile")]
fn parse_level_filter<S: AsRef<str>>(s: S) -> Result<LevelFilter, FlexiLoggerError> {
    match s.as_ref().to_lowercase().as_ref() {
        "off" => Ok(LevelFilter::Off),
        "error" => Ok(LevelFilter::Error),
        "warn" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        _ => Err(FlexiLoggerError::LevelFilter(format!(
            "unknown level filter: {}",
            s.as_ref()
        ))),
    }
}

fn contains_dash_or_whitespace(s: &str, parse_errs: &mut Vec<String>) -> bool {
    let result = s.find('-').is_some() || s.find(' ').is_some() || s.find('\t').is_some();
    if result {
        push_err(
            format!(
                "ignoring invalid part in log spec '{}' (contains a dash or whitespace)",
                s
            ),
            parse_errs,
        );
    }
    result
}

/// Builder for `LogSpecification`.
///
/// # Example
///
/// Use the reconfigurability feature and build the log spec programmatically.
///
/// ```rust
/// use flexi_logger::{Logger, LogSpecBuilder};
/// use log::LevelFilter;
///
/// fn main() {
///     // Build the initial log specification
///     let mut builder = LogSpecBuilder::new();  // default is LevelFilter::Off
///     builder.default(LevelFilter::Info);
///     builder.module("karl", LevelFilter::Debug);
///
///     // Initialize Logger, keep builder alive
///     let mut logger_reconf_handle = Logger::with(builder.build())
///         // your logger configuration goes here, as usual
///         .start()
///         .unwrap_or_else(|e| panic!("Logger initialization failed with {}", e));
///
///     // ...
///
///     // Modify builder and update the logger
///     builder.default(LevelFilter::Error);
///     builder.remove("karl");
///     builder.module("emma", LevelFilter::Trace);
///
///     logger_reconf_handle.set_new_spec(builder.build());
///
///     // ...
/// }
/// ```
#[derive(Clone, Default)]
pub struct LogSpecBuilder {
    module_filters: HashMap<Option<String>, LevelFilter>,
}

impl LogSpecBuilder {
    /// Creates a LogSpecBuilder with all logging turned off.
    pub fn new() -> LogSpecBuilder {
        let mut modfilmap = HashMap::new();
        modfilmap.insert(None, LevelFilter::Off);
        LogSpecBuilder {
            module_filters: modfilmap,
        }
    }

    /// Creates a LogSpecBuilder from given module filters.
    pub fn from_module_filters(module_filters: &[ModuleFilter]) -> LogSpecBuilder {
        let mut modfilmap = HashMap::new();
        for mf in module_filters {
            modfilmap.insert(mf.module_name.clone(), mf.level_filter);
        }
        LogSpecBuilder {
            module_filters: modfilmap,
        }
    }

    /// Adds a default log level filter, or updates the default log level filter.
    pub fn default(&mut self, lf: LevelFilter) -> &mut LogSpecBuilder {
        self.module_filters.insert(None, lf);
        self
    }

    /// Adds a log level filter, or updates the log level filter, for a module.
    pub fn module<M: AsRef<str>>(
        &mut self,
        module_name: M,
        lf: LevelFilter,
    ) -> &mut LogSpecBuilder {
        self.module_filters
            .insert(Some(module_name.as_ref().to_owned()), lf);
        self
    }

    /// Adds a log level filter, or updates the log level filter, for a module.
    pub fn remove<M: AsRef<str>>(&mut self, module_name: M) -> &mut LogSpecBuilder {
        self.module_filters
            .remove(&Some(module_name.as_ref().to_owned()));
        self
    }

    /// Creates a log specification without text filter.
    pub fn finalize(self) -> LogSpecification {
        LogSpecification {
            module_filters: self.module_filters.into_vec_module_filter(),
            textfilter: None,
        }
    }

    /// Creates a log specification with text filter.
    pub fn finalize_with_textfilter(self, tf: Regex) -> LogSpecification {
        LogSpecification {
            module_filters: self.module_filters.into_vec_module_filter(),
            textfilter: Some(tf),
        }
    }

    /// Creates a log specification without being consumed.
    pub fn build(&self) -> LogSpecification {
        LogSpecification {
            module_filters: self.module_filters.clone().into_vec_module_filter(),
            textfilter: None,
        }
    }

    /// Creates a log specification without being consumed, optionally with a text filter.
    pub fn build_with_textfilter(&self, tf: Option<Regex>) -> LogSpecification {
        LogSpecification {
            module_filters: self.module_filters.clone().into_vec_module_filter(),
            textfilter: tf,
        }
    }
}

trait IntoVecModuleFilter {
    fn into_vec_module_filter(self) -> Vec<ModuleFilter>;
}
impl IntoVecModuleFilter for HashMap<Option<String>, LevelFilter> {
    fn into_vec_module_filter(self) -> Vec<ModuleFilter> {
        let mf: Vec<ModuleFilter> = self
            .into_iter()
            .map(|(k, v)| ModuleFilter {
                module_name: k,
                level_filter: v,
            })
            .collect();
        mf.level_sort()
    }
}

trait LevelSort {
    fn level_sort(self) -> Vec<ModuleFilter>;
}
impl LevelSort for Vec<ModuleFilter> {
    /// Sort the module filters by length of their name,
    /// this allows a little more efficient lookup at runtime.
    fn level_sort(mut self) -> Vec<ModuleFilter> {
        self.sort_by(|a, b| {
            let alen = a.module_name.as_ref().map(|a| a.len()).unwrap_or(0);
            let blen = b.module_name.as_ref().map(|b| b.len()).unwrap_or(0);
            blen.cmp(&alen)
        });
        self
    }
}

#[cfg(features = "specfile")]
#[cfg(test)]
mod tests {
    use log::{Level, LevelFilter};
    use {LogSpecBuilder, LogSpecification};

    #[test]
    fn specfile() {
        compare_specs(
            "[modules]\n\
             ",
            "",
        );

        compare_specs(
            "global_level = 'info'\n\
             \n\
             [modules]\n\
             ",
            "info",
        );

        compare_specs(
            "global_level = 'info'\n\
             \n\
             [modules]\n\
             'mod1::mod2' = 'debug'\n\
             'mod3' = 'trace'\n\
             ",
            "info, mod1::mod2 = debug, mod3 = trace",
        );

        compare_specs(
            "global_level = 'info'\n\
             global_pattern = 'Foo'\n\
             \n\
             [modules]\n\
             'mod1::mod2' = 'debug'\n\
             'mod3' = 'trace'\n\
             ",
            "info, mod1::mod2 = debug, mod3 = trace /Foo",
        );
    }

    fn compare_specs(s1: &str, s2: &str) {
        let ls1 = LogSpecification::from_toml(s1).unwrap();
        let ls2 = LogSpecification::parse(s2);

        assert_eq!(ls1.module_filters, ls2.module_filters);
        assert_eq!(ls1.textfilter.is_none(), ls2.textfilter.is_none());
        if ls1.textfilter.is_some() && ls2.textfilter.is_some() {
            assert_eq!(
                ls1.textfilter.unwrap().to_string(),
                ls2.textfilter.unwrap().to_string()
            );
        }
    }

    #[test]
    fn parse_logging_spec_valid() {
        let spec = LogSpecification::parse("crate1::mod1=error,crate1::mod2,crate2=debug");
        assert_eq!(spec.module_filters().len(), 3);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate1::mod1".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Error);

        assert_eq!(
            spec.module_filters()[1].module_name,
            Some("crate1::mod2".to_string())
        );
        assert_eq!(spec.module_filters()[1].level_filter, LevelFilter::max());

        assert_eq!(
            spec.module_filters()[2].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[2].level_filter, LevelFilter::Debug);

        assert!(spec.text_filter().is_none());
    }

    #[test]
    fn parse_logging_spec_invalid_crate() {
        // test parse_logging_spec with multiple = in specification
        let spec = LogSpecification::parse("crate1::mod1=warn=info,crate2=debug");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Debug);
        assert!(spec.text_filter().is_none());
    }

    #[test]
    fn parse_logging_spec_invalid_log_level() {
        // test parse_logging_spec with 'noNumber' as log level
        let spec = LogSpecification::parse("crate1::mod1=noNumber,crate2=debug");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Debug);
        assert!(spec.text_filter().is_none());
    }

    #[test]
    fn parse_logging_spec_string_log_level() {
        // test parse_logging_spec with 'warn' as log level
        let spec = LogSpecification::parse("crate1::mod1=wrong, crate2=warn");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Warn);
        assert!(spec.text_filter().is_none());
    }

    #[test]
    fn parse_logging_spec_empty_log_level() {
        // test parse_logging_spec with '' as log level
        let spec = LogSpecification::parse("crate1::mod1=wrong, crate2=");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::max());
        assert!(spec.text_filter().is_none());
    }

    #[test]
    fn parse_logging_spec_global() {
        // test parse_logging_spec with no crate
        let spec = LogSpecification::parse("warn,crate2=debug");
        assert_eq!(spec.module_filters().len(), 2);

        assert_eq!(spec.module_filters()[1].module_name, None);
        assert_eq!(spec.module_filters()[1].level_filter, LevelFilter::Warn);

        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Debug);

        assert!(spec.text_filter().is_none());
    }

    #[test]
    fn parse_logging_spec_valid_filter() {
        let spec = LogSpecification::parse(" crate1::mod1 = error , crate1::mod2,crate2=debug/abc");
        assert_eq!(spec.module_filters().len(), 3);

        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate1::mod1".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Error);

        assert_eq!(
            spec.module_filters()[1].module_name,
            Some("crate1::mod2".to_string())
        );
        assert_eq!(spec.module_filters()[1].level_filter, LevelFilter::max());

        assert_eq!(
            spec.module_filters()[2].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[2].level_filter, LevelFilter::Debug);
        assert!(
            spec.text_filter().is_some()
                && spec.text_filter().as_ref().unwrap().to_string() == "abc"
        );
    }

    #[test]
    fn parse_logging_spec_invalid_crate_filter() {
        let spec = LogSpecification::parse("crate1::mod1=error=warn,crate2=debug/a.c");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Debug);
        assert!(
            spec.text_filter().is_some()
                && spec.text_filter().as_ref().unwrap().to_string() == "a.c"
        );
    }

    #[test]
    fn parse_logging_spec_invalid_crate_with_dash() {
        let spec = LogSpecification::parse("karl-heinz::mod1=warn,crate2=debug/a.c");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate2".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::Debug);
        assert!(
            spec.text_filter().is_some()
                && spec.text_filter().as_ref().unwrap().to_string() == "a.c"
        );
    }

    #[test]
    fn parse_logging_spec_empty_with_filter() {
        let spec = LogSpecification::parse("crate1/a*c");
        assert_eq!(spec.module_filters().len(), 1);
        assert_eq!(
            spec.module_filters()[0].module_name,
            Some("crate1".to_string())
        );
        assert_eq!(spec.module_filters()[0].level_filter, LevelFilter::max());
        assert!(
            spec.text_filter().is_some()
                && spec.text_filter().as_ref().unwrap().to_string() == "a*c"
        );
    }

    #[test]
    fn reuse_logspec_builder() {
        let mut builder = LogSpecBuilder::new();

        builder.default(LevelFilter::Info);
        builder.module("carlo", LevelFilter::Debug);
        builder.module("toni", LevelFilter::Warn);
        let spec1 = builder.build();

        assert_eq!(
            spec1.module_filters()[0].module_name,
            Some("carlo".to_string())
        );
        assert_eq!(spec1.module_filters()[0].level_filter, LevelFilter::Debug);

        assert_eq!(
            spec1.module_filters()[1].module_name,
            Some("toni".to_string())
        );
        assert_eq!(spec1.module_filters()[1].level_filter, LevelFilter::Warn);

        assert_eq!(spec1.module_filters().len(), 3);
        assert_eq!(spec1.module_filters()[2].module_name, None);
        assert_eq!(spec1.module_filters()[2].level_filter, LevelFilter::Info);

        builder.default(LevelFilter::Error);
        builder.remove("carlo");
        builder.module("greta", LevelFilter::Trace);
        let spec2 = builder.build();

        assert_eq!(spec2.module_filters().len(), 3);
        assert_eq!(spec2.module_filters()[2].module_name, None);
        assert_eq!(spec2.module_filters()[2].level_filter, LevelFilter::Error);

        assert_eq!(
            spec2.module_filters()[0].module_name,
            Some("greta".to_string())
        );
        assert_eq!(spec2.module_filters()[0].level_filter, LevelFilter::Trace);

        assert_eq!(
            spec2.module_filters()[1].module_name,
            Some("toni".to_string())
        );
        assert_eq!(spec2.module_filters()[1].level_filter, LevelFilter::Warn);
    }

    ///////////////////////////////////////////////////////
    ///////////////////////////////////////////////////////
    #[test]
    fn match_full_path() {
        let spec = LogSpecification::parse("crate2=info,crate1::mod1=warn");
        assert!(spec.enabled(Level::Warn, "crate1::mod1"));
        assert!(!spec.enabled(Level::Info, "crate1::mod1"));
        assert!(spec.enabled(Level::Info, "crate2"));
        assert!(!spec.enabled(Level::Debug, "crate2"));
    }

    #[test]
    fn no_match() {
        let spec = LogSpecification::parse("crate2=info,crate1::mod1=warn");
        assert!(!spec.enabled(Level::Warn, "crate3"));
    }

    #[test]
    fn match_beginning() {
        let spec = LogSpecification::parse("crate2=info,crate1::mod1=warn");
        assert!(spec.enabled(Level::Info, "crate2::mod1"));
    }

    #[test]
    fn match_beginning_longest_match() {
        let spec = LogSpecification::parse(
            "abcd = info, abcd::mod1 = error, klmn::mod = debug, klmn = info",
        );
        assert!(spec.enabled(Level::Error, "abcd::mod1::foo"));
        assert!(!spec.enabled(Level::Warn, "abcd::mod1::foo"));
        assert!(spec.enabled(Level::Warn, "abcd::mod2::foo"));
        assert!(!spec.enabled(Level::Debug, "abcd::mod2::foo"));

        assert!(!spec.enabled(Level::Debug, "klmn"));
        assert!(!spec.enabled(Level::Debug, "klmn::foo::bar"));
        assert!(spec.enabled(Level::Info, "klmn::foo::bar"));
    }

    #[test]
    fn match_default1() {
        let spec = LogSpecification::parse("info,abcd::mod1=warn");
        assert!(spec.enabled(Level::Warn, "abcd::mod1"));
        assert!(spec.enabled(Level::Info, "crate2::mod2"));
    }

    #[test]
    fn match_default2() {
        let spec = LogSpecification::parse("modxyz=error, info, abcd::mod1=warn");
        assert!(spec.enabled(Level::Warn, "abcd::mod1"));
        assert!(spec.enabled(Level::Info, "crate2::mod2"));
    }

    #[test]
    fn zero_level() {
        let spec = LogSpecification::parse("info,crate1::mod1=off");
        assert!(!spec.enabled(Level::Error, "crate1::mod1"));
        assert!(spec.enabled(Level::Info, "crate2::mod2"));
    }

}
