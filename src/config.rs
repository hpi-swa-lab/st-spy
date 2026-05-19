#[cfg(feature = "cli")]
use clap::builder::{styling::AnsiColor, EnumValueParser, Styles};
#[cfg(feature = "cli")]
use clap::{
    crate_description, crate_name, crate_version, value_parser, Arg, ArgAction, Command, ValueEnum,
};
use remoteprocess::Pid;

/// Options on how to collect samples from an OpenSmalltalk VM process.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Whether or not we should stop the target process when taking samples.
    /// Setting this to false reduces sampling impact but can lead to partial stacks.
    pub blocking: LockingStrategy,

    #[doc(hidden)]
    pub command: String,
    #[doc(hidden)]
    pub pid: Option<Pid>,
    #[doc(hidden)]
    pub program: Option<Vec<String>>,
    #[doc(hidden)]
    pub sampling_rate: u64,
    #[doc(hidden)]
    pub filename: Option<String>,
    #[doc(hidden)]
    pub format: Option<FileFormat>,
    #[doc(hidden)]
    pub show_line_numbers: bool,
    #[doc(hidden)]
    pub duration: RecordDuration,
    #[doc(hidden)]
    pub include_idle: bool,
    #[doc(hidden)]
    pub include_thread_ids: bool,
    #[doc(hidden)]
    pub subprocesses: bool,
    #[doc(hidden)]
    pub hide_progress: bool,
    #[doc(hidden)]
    pub capture_output: bool,
    #[doc(hidden)]
    pub dump_json: bool,
    #[doc(hidden)]
    pub full_filenames: bool,
    #[doc(hidden)]
    pub refresh_seconds: f64,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "cli", derive(ValueEnum))]
pub enum FileFormat {
    flamegraph,
    raw,
    speedscope,
    chrometrace,
}

#[cfg(feature = "cli")]
impl std::str::FromStr for FileFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for variant in Self::value_variants() {
            if variant.to_possible_value().unwrap().matches(s, false) {
                return Ok(*variant);
            }
        }
        Err(format!("Invalid fileformat: {s}"))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LockingStrategy {
    #[allow(dead_code)]
    AlreadyLocked,
    Lock,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RecordDuration {
    Unlimited,
    Seconds(u64),
}

impl Default for Config {
    fn default() -> Config {
        Config {
            pid: None,
            program: None,
            filename: None,
            format: None,
            command: String::from("top"),
            blocking: LockingStrategy::Lock,
            show_line_numbers: false,
            sampling_rate: 100,
            duration: RecordDuration::Unlimited,
            include_idle: false,
            include_thread_ids: false,
            hide_progress: false,
            capture_output: true,
            dump_json: false,
            subprocesses: false,
            full_filenames: false,
            refresh_seconds: 1.0,
        }
    }
}

#[cfg(feature = "cli")]
impl Config {
    /// Uses clap to set config options from command line arguments.
    pub fn from_commandline() -> Config {
        let args: Vec<String> = std::env::args().collect();
        Config::from_args(&args).unwrap_or_else(|e| e.exit())
    }

    pub fn from_args(args: &[String]) -> clap::error::Result<Config> {
        let pid = Arg::new("pid")
            .short('p')
            .long("pid")
            .value_name("pid")
            .help("PID of a running OpenSmalltalk VM to sample, in decimal or hex")
            .action(ArgAction::Set);

        #[cfg(not(target_os = "freebsd"))]
        let nonblocking = Arg::new("nonblocking")
            .long("nonblocking")
            .help(
                "Don't pause the target process when collecting samples. This reduces sampling \
                 impact, but may produce inaccurate stacks",
            )
            .action(ArgAction::SetTrue);

        let rate = Arg::new("rate")
            .short('r')
            .long("rate")
            .value_name("rate")
            .help("The number of samples to collect per second")
            .default_value("100")
            .value_parser(value_parser!(u64))
            .action(ArgAction::Set);

        let subprocesses = Arg::new("subprocesses")
            .short('s')
            .long("subprocesses")
            .help("Profile subprocesses of the original process")
            .action(ArgAction::SetTrue);

        let full_filenames = Arg::new("full_filenames")
            .long("full-filenames")
            .help("Show full source filenames instead of shortening to the basename")
            .action(ArgAction::SetTrue);

        let program = Arg::new("program")
            .help("Command line of an OpenSmalltalk VM to run")
            .num_args(1..)
            .trailing_var_arg(true)
            .allow_hyphen_values(true)
            .action(ArgAction::Append);

        let idle = Arg::new("idle")
            .short('i')
            .long("idle")
            .help("Include stack traces for idle threads")
            .action(ArgAction::SetTrue);

        let top_delay = Arg::new("delay")
            .long("delay")
            .value_name("seconds")
            .help("Delay between top refreshes")
            .default_value("1.0")
            .value_parser(clap::value_parser!(f64))
            .action(ArgAction::Set);

        let record = Command::new("record")
            .about("Record stack traces to a flamegraph, speedscope, raw, or chrome trace file")
            .arg(program.clone())
            .arg(pid.clone().required_unless_present("program"))
            .arg(full_filenames.clone())
            .arg(
                Arg::new("output")
                    .short('o')
                    .long("output")
                    .value_name("filename")
                    .help("Output filename")
                    .action(ArgAction::Set)
                    .required(false),
            )
            .arg(
                Arg::new("format")
                    .short('f')
                    .long("format")
                    .value_name("format")
                    .help("Output file format")
                    .action(ArgAction::Set)
                    .value_parser(EnumValueParser::<FileFormat>::new())
                    .ignore_case(true)
                    .default_value("flamegraph"),
            )
            .arg(
                Arg::new("duration")
                    .short('d')
                    .long("duration")
                    .value_name("duration")
                    .help("The number of seconds to sample for")
                    .default_value("unlimited")
                    .action(ArgAction::Set),
            )
            .arg(rate.clone())
            .arg(subprocesses.clone())
            .arg(
                Arg::new("nolineno")
                    .long("nolineno")
                    .help("Do not show line numbers")
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("threads")
                    .short('t')
                    .long("threads")
                    .help("Show thread ids in the output")
                    .action(ArgAction::SetTrue),
            )
            .arg(idle.clone())
            .arg(
                Arg::new("capture")
                    .long("capture")
                    .hide(true)
                    .help("Capture output from child process")
                    .action(ArgAction::SetTrue),
            )
            .arg(
                Arg::new("hideprogress")
                    .long("hideprogress")
                    .hide(true)
                    .help("Hide progress bar")
                    .action(ArgAction::SetTrue),
            );

        let top = Command::new("top")
            .about("Display a top-like view of functions consuming CPU")
            .arg(program.clone())
            .arg(pid.clone().required_unless_present("program"))
            .arg(rate.clone())
            .arg(subprocesses.clone())
            .arg(full_filenames.clone())
            .arg(idle.clone())
            .arg(top_delay.clone());

        let dump = Command::new("dump")
            .about("Dump stack traces for a target VM to stdout")
            .arg(pid.clone().required(true))
            .arg(full_filenames.clone())
            .arg(
                Arg::new("json")
                    .short('j')
                    .long("json")
                    .help("Format output as JSON")
                    .action(ArgAction::SetTrue),
            )
            .arg(subprocesses.clone());

        let completions = Command::new("completions")
            .about("Generate shell completions")
            .hide(true)
            .arg(
                Arg::new("shell")
                    .value_parser(value_parser!(clap_complete::Shell))
                    .help("Shell type")
                    .required(true)
                    .action(ArgAction::Set),
            );

        #[cfg(not(target_os = "freebsd"))]
        let record = record.arg(nonblocking.clone());
        #[cfg(not(target_os = "freebsd"))]
        let top = top.arg(nonblocking.clone());
        #[cfg(not(target_os = "freebsd"))]
        let dump = dump.arg(nonblocking.clone());

        let styles = Styles::styled()
            .header(AnsiColor::Yellow.on_default())
            .usage(AnsiColor::Yellow.on_default())
            .literal(AnsiColor::Green.on_default())
            .placeholder(AnsiColor::Green.on_default());

        let mut app = Command::new(crate_name!())
            .version(crate_version!())
            .about(crate_description!())
            .subcommand_required(true)
            .infer_subcommands(true)
            .arg_required_else_help(true)
            .styles(styles)
            .subcommand(record)
            .subcommand(top)
            .subcommand(dump)
            .subcommand(completions);
        let matches = app.clone().try_get_matches_from(args)?;
        debug!("Command line args: {:?}", matches);

        let mut config = Config::default();
        let (subcommand, matches) = matches.subcommand().unwrap();

        match subcommand {
            "record" => {
                config.sampling_rate = *matches.get_one("rate").unwrap();
                config.duration = match matches.get_one::<String>("duration").map(|d| d.as_str()) {
                    Some("unlimited") | None => RecordDuration::Unlimited,
                    Some(seconds) => {
                        RecordDuration::Seconds(seconds.parse().expect("invalid duration"))
                    }
                };
                config.format = matches.get_one("format").copied();
                config.filename = matches.get_one::<String>("output").cloned();
                config.show_line_numbers = !matches.get_flag("nolineno");
                config.include_thread_ids = matches.get_flag("threads");
                config.hide_progress = matches.get_flag("hideprogress");
            }
            "top" => {
                config.sampling_rate = *matches.get_one("rate").unwrap();
                config.refresh_seconds = *matches.get_one::<f64>("delay").unwrap();
            }
            "dump" => {
                config.dump_json = matches.get_flag("json");
            }
            "completions" => {
                let shell = matches.get_one::<clap_complete::Shell>("shell").unwrap();
                let app_name = app.get_name().to_string();
                clap_complete::generate(*shell, &mut app, app_name, &mut std::io::stdout());
                std::process::exit(0);
            }
            _ => {}
        }

        match subcommand {
            "record" | "top" => {
                config.program = matches
                    .get_many::<String>("program")
                    .map(|vals| vals.map(|v| v.to_owned()).collect());
                config.include_idle = matches.get_flag("idle");
            }
            _ => {}
        }

        config.subprocesses = matches.get_flag("subprocesses");
        config.command = subcommand.to_owned();

        config.pid =
            matches
                .get_one::<String>("pid")
                .map(|p| match p.to_lowercase().strip_prefix("0x") {
                    Some(prefix) => Pid::from_str_radix(prefix, 16).expect("invalid pid"),
                    None => p.parse().expect("invalid pid"),
                });

        config.full_filenames = matches.get_flag("full_filenames");
        config.capture_output = config.command != "record" || matches.get_flag("capture");
        if !config.capture_output {
            config.hide_progress = true;
        }

        #[cfg(not(target_os = "freebsd"))]
        if matches.get_flag("nonblocking") {
            eprintln!("st-spy requires process suspension to unwind OpenSmalltalk VM stacks.");
            std::process::exit(1);
        }

        #[cfg(target_os = "freebsd")]
        {
            if config.pid.is_some() && std::env::var("STSPY_ALLOW_FREEBSD_ATTACH").is_err() {
                eprintln!("On FreeBSD, attaching to a running VM can disrupt the target process.");
                eprintln!(
                    "Set STSPY_ALLOW_FREEBSD_ATTACH=1 to acknowledge that risk and continue."
                );
                std::process::exit(-1);
            }
        }

        info!("config {:#?}", config);
        Ok(config)
    }
}

#[cfg(feature = "cli")]
#[cfg(test)]
mod tests {
    use super::*;

    fn get_config(cmd: &str) -> clap::error::Result<Config> {
        #[cfg(target_os = "freebsd")]
        std::env::set_var("STSPY_ALLOW_FREEBSD_ATTACH", "1");
        let args: Vec<String> = cmd.split_whitespace().map(|x| x.to_owned()).collect();
        Config::from_args(&args)
    }

    #[test]
    fn test_parse_record_args() {
        let config = get_config("st-spy record --pid 1234 --output foo").unwrap();
        assert_eq!(config.pid, Some(1234));
        assert_eq!(config.filename, Some(String::from("foo")));
        assert_eq!(config.format, Some(FileFormat::flamegraph));
        assert_eq!(config.command, String::from("record"));

        let short_config = get_config("st-spy r -p 1234 -o foo").unwrap();
        assert_eq!(config, short_config);

        assert_eq!(
            get_config("st-spy record -o foo").unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let program_config = get_config("st-spy r -o foo -- squeak Squeak.image").unwrap();
        assert_eq!(
            program_config.program,
            Some(vec![String::from("squeak"), String::from("Squeak.image")])
        );
        assert_eq!(program_config.pid, None);

        assert_eq!(
            get_config("st-spy r -p 1234 -o foo -f unknown")
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::InvalidValue
        );

        assert!(!config.include_idle);
        assert!(!config.include_thread_ids);

        let config_flags = get_config("st-spy r -p 1234 -o foo --idle --threads").unwrap();
        assert!(config_flags.include_idle);
        assert!(config_flags.include_thread_ids);
    }

    #[test]
    fn test_parse_dump_args() {
        let config = get_config("st-spy dump --pid 1234").unwrap();
        assert_eq!(config.pid, Some(1234));
        assert_eq!(config.command, String::from("dump"));

        let short_config = get_config("st-spy d -p 1234").unwrap();
        assert_eq!(config, short_config);

        assert_eq!(
            get_config("st-spy dump").unwrap_err().kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn test_parse_top_args() {
        let config = get_config("st-spy top --pid 1234").unwrap();
        assert_eq!(config.pid, Some(1234));
        assert_eq!(config.command, String::from("top"));

        let short_config = get_config("st-spy t -p 1234").unwrap();
        assert_eq!(config, short_config);
    }

    #[test]
    fn test_parse_args() {
        assert_eq!(
            get_config("st-spy dude").unwrap_err().kind(),
            clap::error::ErrorKind::InvalidSubcommand
        );
    }
}
