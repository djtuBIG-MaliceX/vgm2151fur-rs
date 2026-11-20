//! The command line: convert, report, edit, compare, render, menu.
//!
//! Literal port of `vgm2151fur/__main__.py` at v0.11.0. argparse is not
//! available here, so the parser below reproduces its flags, defaults, help
//! text and error wording by hand. The browser GUI (`gui`) is out of scope.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::convert::{
    ConvertInfo, ConvertOptions, ConvertRow, ReportOptions, VGM_SUFFIXES,
    convert_folder, format_variant_lines, report_files,
};
use crate::edit::Chip;
use crate::error::{Error, Result};
use crate::paths::{clean_text, dedupe, expand_token, fur_targets, vgm_targets};

pub const COMMANDS: [&str; 6] = ["convert", "report", "edit", "compare", "render", "menu"];

pub const _WORKERS_HELP: &str = "tracks at once, at low priority (default: logical CPUs minus one or two \
threads, capped at 16; requests cap at threads minus one, 31 max)";

pub const EPILOG: &str = r#"paths:
  A .vgm/.vgz file, a pack folder, or a folder of packs. Quotes around a
  pasted path are stripped. A glob the shell left unexpanded (*.vgz) is
  expanded here. edit and render also take a .fur file, a folder of them,
  or the pack folder that holds fur/.

  vgm2151fur "Song.vgz"
  vgm2151fur "Pack Folder" -o out
  vgm2151fur convert "Song.vgz" --no-normalize
  vgm2151fur edit "Pack Folder" --chip C140 --factor 0.9
  vgm2151fur render "Pack Folder" --loops 2
"#;

pub const DESCRIPTION: &str = "Transcribe YM2151 (OPM), YM2203 (OPN), YM2612 (OPN2), YM2610 and \
OPL-family (YM3526/YM3812/YMF262) + sample-chip (K007232, K054539, MSM6295, MSM6258, SegaPCM, C140, \
C219, C352, K053260, YMZ280B, YMW258-F MultiPCM, uPD7759, 32X PWM) + Nintendo Virtual Boy (VSU) + \
Philips SAA1099 (Creative Music System / Game Blaster) + Atari POKEY and Lynx/Mikey + Seta X1-010 + \
Nintendo FDS + Bandai WonderSwan + Capcom QSound VGMs into Furnace .fur files, and edit the result.";

const MAIN_USAGE: &str = "usage: vgm2151fur [-h] [--version]
                  {convert,report,edit,compare,render,menu} ...\n";

const MAIN_HELP: &str = "usage: vgm2151fur [-h] [--version]
                  {convert,report,edit,compare,render,menu} ...

Transcribe YM2151 (OPM), YM2203 (OPN), YM2612 (OPN2), YM2610 and OPL-family (YM3526/YM3812/YMF262) + sample-chip (K007232, K054539, MSM6295, MSM6258, SegaPCM, C140, C219, C352, K053260, YMZ280B, YMW258-F MultiPCM, uPD7759, 32X PWM) + Nintendo Virtual Boy (VSU) + Philips SAA1099 (Creative Music System / Game Blaster) + Atari POKEY and Lynx/Mikey + Seta X1-010 + Nintendo FDS + Bandai WonderSwan + Capcom QSound VGMs into Furnace .fur files, and edit the result.

positional arguments:
  {convert,report,edit,compare,render,menu}
    convert             convert VGM/VGZ files or folders
    report              print the analysis of a VGM without writing
    edit                list or change .fur chip volumes
    compare             pitch-check a .fur against its source VGM
    render              render .fur files to WAV with Furnace
    menu                interactive menu

options:
  -h, --help            show this help message and exit
  --version             show program's version number and exit

paths:
  A .vgm/.vgz file, a pack folder, or a folder of packs. Quotes around a
  pasted path are stripped. A glob the shell left unexpanded (*.vgz) is
  expanded here. edit and render also take a .fur file, a folder of them,
  or the pack folder that holds fur/.

  vgm2151fur \"Song.vgz\"
  vgm2151fur \"Pack Folder\" -o out
  vgm2151fur convert \"Song.vgz\" --no-normalize
  vgm2151fur edit \"Pack Folder\" --chip C140 --factor 0.9
  vgm2151fur render \"Pack Folder\" --loops 2\n";

const CONVERT_USAGE: &str = "usage: vgm2151fur convert [-h] [-o OUT] [--speed SPEED] [--max-bpm MAX_BPM]
                          [--variants] [--condense N]
                          [--optimize {none,lossless,all}] [--verify]
                          [--factors LIST] [--dirs] [--workers WORKERS]
                          [--c352-quad] [--melody-only] [--pcm-only]
                          [--no-normalize] [--no-loop-find] [--no-lfo-loop]
                          [--lossy-sample-clustering]
                          PATH [PATH ...]\n";

const CONVERT_HELP: &str = "usage: vgm2151fur convert [-h] [-o OUT] [--speed SPEED] [--max-bpm MAX_BPM]
                          [--variants] [--condense N]
                          [--optimize {none,lossless,all}] [--verify]
                          [--factors LIST] [--dirs] [--workers WORKERS]
                          [--c352-quad] [--melody-only] [--pcm-only]
                          [--no-normalize] [--no-loop-find] [--no-lfo-loop]
                          [--lossy-sample-clustering]
                          PATH [PATH ...]

positional arguments:
  PATH                  VGM/VGZ file, pack folder, or folder of packs

options:
  -h, --help            show this help message and exit
  -o, --out OUT         output folder (default: <input>/fur)
  --speed SPEED         ticks per row (default: auto)
  --max-bpm MAX_BPM     cap the tracker BPM (Furnace shows 15 x rows/s) by
                        using the coarsest row grid under the cap; halving it
                        halves the row rate. Slides change rate at most once
                        per row, so a lower cap trades slide detail for
                        readability
  --variants            write the 1x..4x row-condense set side by side
                        (<name>.fur plus '<name> xN.fur'); never replaces the
                        1x file
  --condense N          largest condense factor for the variants (default 4).
                        1x = today's grid, Nx = N times longer rows. The
                        ladder keeps the half steps (1, 1.5, 2, ...) because a
                        musical unit that lands between whole steps would
                        otherwise be skipped
  --optimize {none,lossless,all}
                        which variant to pick: none (1x only), lossless
                        (coarsest with no added structural loss and <=0.1 st
                        more pitch deviation, else 1x), all (largest factor)
  --verify              with lossless: render every variant through Furnace
                        and reject any that adds a sustained pitch divergence
                        over 1x (0.25 st / 60 ms). Slower, but catches what
                        the structural counters miss
  --factors LIST        explicit condense factors to write, e.g. 1,1.5,2,3,4
                        (fractional factors allowed); overrides --condense
  --dirs                write variants as default/ and xN optimised/
                        subfolders (the kept factors) plus a condense.tsv
                        manifest, instead of the flat '<name> xN.fur' siblings
  --workers WORKERS     tracks at once, at low priority (default: logical CPUs
                        minus one or two threads, capped at 16; requests cap
                        at threads minus one, 31 max)
  --c352-quad           keep a Namco C352 source's rear pair on its own
                        outputs instead of folding it into the front pair, and
                        set the chip's quadOutput flag. Only tracks whose
                        source drives the rear registers change; a stereo
                        mixdown of the result sums the pairs back together
  --melody-only         skip sample chips
  --pcm-only            skip the YM2151/YM2203/YM2612/YM2610 and OPL-family FM
  --no-normalize        skip the -2.5 dBFS volume fit (it renders each track
                        once)
  --no-loop-find        keep the VGM loop offset instead of moving it onto the
                        bar
  --no-lfo-loop         skip the LFO/tail loop search for long pitch macros
                        (faster; exact first-cycle loops are still detected)
  --lossy-sample-clustering
                        experimental, lossy: reduce the YM2612 DAC, SSG PCM
                        and NES raw PCM maps to about 32 bodies by mapping
                        short fragments to similar long/hot samples\n";

const REPORT_USAGE: &str = "usage: vgm2151fur report [-h] [--speed SPEED] [--max-bpm MAX_BPM]
                         [--workers WORKERS]
                         PATH [PATH ...]\n";

const REPORT_HELP: &str = "usage: vgm2151fur report [-h] [--speed SPEED] [--max-bpm MAX_BPM]
                         [--workers WORKERS]
                         PATH [PATH ...]

positional arguments:
  PATH               VGM/VGZ file or folder

options:
  -h, --help         show this help message and exit
  --speed SPEED
  --max-bpm MAX_BPM
  --workers WORKERS  tracks at once, at low priority (default: logical CPUs
                     minus one or two threads, capped at 16; requests cap at
                     threads minus one, 31 max)\n";

const EDIT_USAGE: &str = "usage: vgm2151fur edit [-h] [--list] [--chip CHIP] [--factor FACTOR]
                       [--value VALUE]
                       PATH [PATH ...]\n";

const EDIT_HELP: &str = "usage: vgm2151fur edit [-h] [--list] [--chip CHIP] [--factor FACTOR]
                       [--value VALUE]
                       PATH [PATH ...]

positional arguments:
  PATH             .fur file, folder, or pack folder

options:
  -h, --help       show this help message and exit
  --list           print the chip table only
  --chip CHIP      chip number (1-based), name, or 'all'
  --factor FACTOR  multiply the volume
  --value VALUE    set the volume outright\n";

const COMPARE_USAGE: &str = "usage: vgm2151fur compare [-h] [--channel CHANNEL] source fur\n";

const COMPARE_HELP: &str = "usage: vgm2151fur compare [-h] [--channel CHANNEL] source fur

positional arguments:
  source             source .vgm/.vgz
  fur                converted .fur

options:
  -h, --help         show this help message and exit
  --channel CHANNEL  module channel (default 0)\n";

const RENDER_USAGE: &str =
    "usage: vgm2151fur render [-h] [-o OUT] [--loops LOOPS] PATH [PATH ...]\n";

const RENDER_HELP: &str = "usage: vgm2151fur render [-h] [-o OUT] [--loops LOOPS] PATH [PATH ...]

positional arguments:
  PATH           .fur file, folder, or pack folder

options:
  -h, --help     show this help message and exit
  -o, --out OUT  output WAV (single input only)
  --loops LOOPS  song loops to render\n";

const MENU_USAGE: &str = "usage: vgm2151fur menu [-h]\n";

const MENU_HELP: &str = "usage: vgm2151fur menu [-h]

options:
  -h, --help  show this help message and exit\n";

const BARE_USAGE: &str = "usage: vgm2151fur [-o OUT] [--speed SPEED] [--max-bpm MAX_BPM] [--variants]
                  [--condense N] [--optimize {none,lossless,all}] [--verify]
                  [--factors LIST] [--dirs] [--workers WORKERS] [--c352-quad]
                  [--melody-only] [--pcm-only] [--no-normalize]
                  [--no-loop-find] [--no-lfo-loop] [--lossy-sample-clustering]
                  [--list] [--chip CHIP] [--factor FACTOR] [--value VALUE]
                  [--loops LOOPS] [--channel CHANNEL]
                  PATH [PATH ...]\n";

// ---------------------------------------------------------------------------
// Argument parsing (argparse behaviour without argparse)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Spec {
    names: &'static [&'static str],
    key: &'static str,
    takes_value: bool,
    choices: Option<&'static [&'static str]>,
}

const fn flag(names: &'static [&'static str], key: &'static str) -> Spec {
    Spec { names, key, takes_value: false, choices: None }
}

const fn value(names: &'static [&'static str], key: &'static str) -> Spec {
    Spec { names, key, takes_value: true, choices: None }
}

const fn choice(
    names: &'static [&'static str],
    key: &'static str,
    choices: &'static [&'static str],
) -> Spec {
    Spec { names, key, takes_value: true, choices: Some(choices) }
}

const OPTIMIZE_CHOICES: [&str; 3] = ["none", "lossless", "all"];

const CONVERT_SPECS: [Spec; 18] = [
    flag(&["-h", "--help"], "help"),
    value(&["-o", "--out"], "out"),
    value(&["--speed"], "speed"),
    value(&["--max-bpm"], "max_bpm"),
    flag(&["--variants"], "variants"),
    value(&["--condense"], "condense"),
    choice(&["--optimize"], "optimize", &OPTIMIZE_CHOICES),
    flag(&["--verify"], "verify"),
    value(&["--factors"], "factors"),
    flag(&["--dirs"], "dirs"),
    value(&["--workers"], "workers"),
    flag(&["--c352-quad"], "c352_quad"),
    flag(&["--melody-only"], "melody_only"),
    flag(&["--pcm-only"], "pcm_only"),
    flag(&["--no-normalize"], "no_normalize"),
    flag(&["--no-loop-find"], "no_loop_find"),
    flag(&["--no-lfo-loop"], "no_lfo_loop"),
    flag(&["--lossy-sample-clustering"], "lossy"),
];

const BARE_SPECS: [Spec; 23] = [
    value(&["-o", "--out"], "out"),
    value(&["--speed"], "speed"),
    value(&["--max-bpm"], "max_bpm"),
    flag(&["--variants"], "variants"),
    value(&["--condense"], "condense"),
    choice(&["--optimize"], "optimize", &OPTIMIZE_CHOICES),
    flag(&["--verify"], "verify"),
    value(&["--factors"], "factors"),
    flag(&["--dirs"], "dirs"),
    value(&["--workers"], "workers"),
    flag(&["--c352-quad"], "c352_quad"),
    flag(&["--melody-only"], "melody_only"),
    flag(&["--pcm-only"], "pcm_only"),
    flag(&["--no-normalize"], "no_normalize"),
    flag(&["--no-loop-find"], "no_loop_find"),
    flag(&["--no-lfo-loop"], "no_lfo_loop"),
    flag(&["--lossy-sample-clustering"], "lossy"),
    flag(&["--list"], "list"),
    value(&["--chip"], "chip"),
    value(&["--factor"], "factor"),
    value(&["--value"], "value"),
    value(&["--loops"], "loops"),
    value(&["--channel"], "channel"),
];

const REPORT_SPECS: [Spec; 4] = [
    flag(&["-h", "--help"], "help"),
    value(&["--speed"], "speed"),
    value(&["--max-bpm"], "max_bpm"),
    value(&["--workers"], "workers"),
];

const EDIT_SPECS: [Spec; 5] = [
    flag(&["-h", "--help"], "help"),
    flag(&["--list"], "list"),
    value(&["--chip"], "chip"),
    value(&["--factor"], "factor"),
    value(&["--value"], "value"),
];

const COMPARE_SPECS: [Spec; 2] =
    [flag(&["-h", "--help"], "help"), value(&["--channel"], "channel")];

const RENDER_SPECS: [Spec; 3] = [
    flag(&["-h", "--help"], "help"),
    value(&["-o", "--out"], "out"),
    value(&["--loops"], "loops"),
];

const MENU_SPECS: [Spec; 1] = [flag(&["-h", "--help"], "help")];

#[derive(Clone, Debug, Default)]
struct Parsed {
    input: Vec<String>,
    positionals: Vec<String>,
    out: Option<String>,
    speed: Option<i32>,
    max_bpm: Option<f64>,
    variants: bool,
    condense: Option<i32>,
    optimize: Option<String>,
    verify: bool,
    factors: Option<String>,
    dirs: bool,
    workers: Option<i32>,
    c352_quad: bool,
    melody_only: bool,
    pcm_only: bool,
    no_normalize: bool,
    no_loop_find: bool,
    no_lfo_loop: bool,
    lossy: bool,
    list: bool,
    chip: Option<String>,
    factor: Option<f64>,
    value: Option<f64>,
    loops: Option<i32>,
    channel: Option<i32>,
    source: String,
    fur: String,
    unknown: Vec<String>,
}

enum ParseOutcome {
    Args(Parsed),
    Help,
    Error(String),
}

fn py_strip(text: &str) -> &str {
    text.trim()
}

fn parse_int(text: &str) -> Option<i64> {
    let cleaned = py_strip(text).replace('_', "");
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<i64>().ok()
}

fn parse_float(text: &str) -> Option<f64> {
    let cleaned = py_strip(text).replace('_', "");
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<f64>().ok()
}

/// `-x` starts a new option; `-`, `-1` and `-.5` are values.
fn is_option_token(token: &str) -> bool {
    token.starts_with('-') && token != "-" && !looks_negative_number(token)
}

/// Argparse's negative-number test: `-1` and `-.5` are positionals.
///
/// Its regex is `^-\d+$|^-\d*\.\d+$`; `-1.vgm` is not a number.
fn looks_negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else { return false };
    if let Ok(_) = rest.parse::<i64>() {
        return !rest.is_empty();
    }
    let Some((whole, fraction)) = rest.split_once('.') else { return false };
    if !whole.chars().all(|ch| ch.is_ascii_digit())
        || !fraction.chars().all(|ch| ch.is_ascii_digit())
        || fraction.is_empty()
    {
        return false;
    }
    !whole.is_empty() || !fraction.is_empty()
}

fn match_spec<'a>(specs: &'a [Spec], token: &str) -> std::result::Result<&'a Spec, String> {
    // Exact match first.
    for spec in specs {
        if spec.names.contains(&token) {
            return Ok(spec);
        }
    }
    if token.starts_with("--") {
        let mut matches: Vec<&Spec> = Vec::new();
        for spec in specs {
            if spec
                .names
                .iter()
                .any(|name| name.starts_with("--") && name.starts_with(token))
            {
                if !matches.iter().any(|m| std::ptr::eq(*m, spec)) {
                    matches.push(spec);
                }
            }
        }
        match matches.len() {
            1 => return Ok(matches[0]),
            0 => {}
            _ => {
                let names: Vec<&str> = matches
                    .iter()
                    .flat_map(|spec| {
                        spec.names
                            .iter()
                            .copied()
                            .filter(|name| name.starts_with(token))
                    })
                    .collect();
                return Err(format!(
                    "ambiguous option: {token} could match {}",
                    names.join(", ")
                ));
            }
        }
    }
    Err(format!("unrecognized arguments: {token}"))
}

fn set_flag(parsed: &mut Parsed, key: &str) {
    match key {
        "variants" => parsed.variants = true,
        "verify" => parsed.verify = true,
        "dirs" => parsed.dirs = true,
        "c352_quad" => parsed.c352_quad = true,
        "melody_only" => parsed.melody_only = true,
        "pcm_only" => parsed.pcm_only = true,
        "no_normalize" => parsed.no_normalize = true,
        "no_loop_find" => parsed.no_loop_find = true,
        "no_lfo_loop" => parsed.no_lfo_loop = true,
        "lossy" => parsed.lossy = true,
        "list" => parsed.list = true,
        _ => {}
    }
}

fn set_value(
    parsed: &mut Parsed,
    key: &str,
    name: &str,
    raw: &str,
) -> std::result::Result<(), String> {
    match key {
        "out" => parsed.out = Some(raw.to_string()),
        "chip" => parsed.chip = Some(raw.to_string()),
        "factors" => parsed.factors = Some(raw.to_string()),
        "speed" | "condense" | "workers" | "loops" | "channel" => {
            let Some(number) = parse_int(raw) else {
                return Err(format!("argument {name}: invalid int value: '{raw}'"));
            };
            if number > i32::MAX as i64 || number < i32::MIN as i64 {
                return Err(format!("argument {name}: invalid int value: '{raw}'"));
            }
            let number = number as i32;
            match key {
                "speed" => parsed.speed = Some(number),
                "condense" => parsed.condense = Some(number),
                "workers" => parsed.workers = Some(number),
                "loops" => parsed.loops = Some(number),
                _ => parsed.channel = Some(number),
            }
        }
        "max_bpm" | "factor" | "value" => {
            let Some(number) = parse_float(raw) else {
                return Err(format!("argument {name}: invalid float value: '{raw}'"));
            };
            match key {
                "max_bpm" => parsed.max_bpm = Some(number),
                "factor" => parsed.factor = Some(number),
                _ => parsed.value = Some(number),
            }
        }
        "optimize" => {
            let Some(choices) = CONVERT_SPECS
                .iter()
                .find(|spec| spec.key == "optimize")
                .and_then(|spec| spec.choices)
            else {
                unreachable!()
            };
            if !choices.contains(&raw) {
                return Err(format!(
                    "argument {name}: invalid choice: '{raw}' (choose from {})",
                    choices.join(", ")
                ));
            }
            parsed.optimize = Some(raw.to_string());
        }
        _ => {}
    }
    Ok(())
}

/// Parse one parser's arguments. `usage` and `prog` only feed the caller's
/// error line.
fn parse_args(specs: &[Spec], argv: &[String]) -> ParseOutcome {
    let mut parsed = Parsed::default();
    let mut only_positional = false;
    let mut i = 0;
    while i < argv.len() {
        let token = &argv[i];
        i += 1;
        if !only_positional && token == "--" {
            only_positional = true;
            continue;
        }
        if !only_positional
            && token.starts_with('-')
            && token != "-"
            && !looks_negative_number(token)
        {
            // `--name=value` splits; a short option may carry its value: -oFOO.
            let (name, inline, short_spec) = if token.starts_with("--") {
                match token.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string()), None),
                    None => (token.clone(), None, None),
                }
            } else {
                let mut found: Option<(&str, String, &Spec)> = None;
                for spec in specs {
                    for short in spec.names.iter().copied() {
                        if short.len() == 2
                            && !short.starts_with("--")
                            && token.starts_with(short)
                        {
                            let rest = token[short.len()..].to_string();
                            found = Some((short, rest, spec));
                            break;
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
                match found {
                    Some((short, rest, spec)) => {
                        let inline = match rest.strip_prefix('=') {
                            Some(stripped) => Some(stripped.to_string()),
                            None if rest.is_empty() => None,
                            None => Some(rest),
                        };
                        (short.to_string(), inline, Some(spec))
                    }
                    None => (token.clone(), None, None),
                }
            };
            let spec = match short_spec {
                Some(spec) => spec,
                None => match match_spec(specs, &name) {
                    Ok(spec) => spec,
                    Err(message) => {
                        if message.starts_with("unrecognized arguments: ") {
                            parsed.unknown.push(token.clone());
                            continue;
                        }
                        return ParseOutcome::Error(message);
                    }
                },
            };
            if spec.key == "help" {
                if name.starts_with("--") {
                    if let Some(inline) = inline {
                        return ParseOutcome::Error(format!(
                            "argument -h/--help: ignored explicit argument '{inline}'"
                        ));
                    }
                }
                return ParseOutcome::Help;
            }
            if !spec.takes_value {
                if let Some(inline) = inline {
                    return ParseOutcome::Error(format!(
                        "argument {name}: ignored explicit argument '{inline}'"
                    ));
                }
                set_flag(&mut parsed, spec.key);
                continue;
            }
            let raw = match inline {
                Some(inline) => inline,
                None => {
                    let value_like =
                        argv.get(i).is_some_and(|next| !is_option_token(next));
                    if value_like {
                        let next = argv[i].clone();
                        i += 1;
                        next
                    } else {
                        return ParseOutcome::Error(format!(
                            "argument {}: expected one argument",
                            spec.names.join("/")
                        ));
                    }
                }
            };
            if let Err(message) = set_value(&mut parsed, spec.key, &name, &raw) {
                return ParseOutcome::Error(message);
            }
            continue;
        }
        parsed.positionals.push(token.clone());
    }
    ParseOutcome::Args(parsed)
}

fn write_out(out: &mut dyn Write, text: &str) {
    let _ = out.write_all(text.as_bytes());
}

fn parser_error(err: &mut dyn Write, usage: &str, prog: &str, message: &str) {
    let _ = err.write_all(usage.as_bytes());
    let _ = writeln!(err, "{prog}: error: {message}");
}

fn required(parsed: &Parsed, count: usize, meta: &str) -> Option<String> {
    if parsed.positionals.len() < count {
        Some(format!(
            "the following arguments are required: {meta}"
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Driver seam
// ---------------------------------------------------------------------------

pub trait CliDriver {
    fn convert_folder(
        &self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<()>;
    fn report_files(
        &self,
        files: &[PathBuf],
        opts: &ReportOptions,
        log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)>;
    fn render_wav(&self, fur: &Path, out: &Path, loops: i64) -> Result<PathBuf>;
    fn read_chips(&self, fur: &Path) -> Result<Vec<Chip>>;
    fn set_chip_volume(
        &self,
        fur: &Path,
        factor: Option<f64>,
        value: Option<f64>,
        targets: Option<&[usize]>,
    ) -> Result<Vec<Chip>>;
    /// The test-only interrupt hook (`KeyboardInterrupt` raised by `_main`).
    fn interrupt(&self) -> bool;
}

/// Production driver: forwards to the real library functions.
pub struct RealDriver;

impl CliDriver for RealDriver {
    fn convert_folder(
        &self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<()> {
        convert_folder(
            files,
            dest,
            opts,
            &crate::convert::RealDriver,
            log,
            on_row,
        )
    }

    fn report_files(
        &self,
        files: &[PathBuf],
        opts: &ReportOptions,
        log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
        report_files(files, opts, log)
    }

    fn render_wav(&self, fur: &Path, out: &Path, loops: i64) -> Result<PathBuf> {
        crate::furnace::render_wav(fur, out, loops, None)
    }

    fn read_chips(&self, fur: &Path) -> Result<Vec<Chip>> {
        crate::edit::read_chips_file(fur)
    }

    fn set_chip_volume(
        &self,
        fur: &Path,
        factor: Option<f64>,
        value: Option<f64>,
        targets: Option<&[usize]>,
    ) -> Result<Vec<Chip>> {
        crate::edit::set_chip_volume_file(fur, factor, value, targets, false)
    }

    fn interrupt(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Python `_tokens`: one string flag value becomes a one-element list.
pub fn tokens(value: Option<&str>) -> Vec<String> {
    value.map(|v| vec![v.to_string()]).unwrap_or_default()
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn resolve(tokens: &[String], err: &mut dyn Write) -> (Vec<PathBuf>, i32) {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut rc = 0;
    if tokens.is_empty() {
        let _ = writeln!(err, "ERROR: give a file or a folder");
        return (Vec::new(), 2);
    }
    for token in tokens {
        let matches = expand_token(token);
        if matches.is_empty() {
            let _ = writeln!(err, "ERROR: no match for {}", clean_text(token));
            rc = 1;
            continue;
        }
        let existing: Vec<PathBuf> = matches.iter().filter(|p| p.exists()).cloned().collect();
        if existing.is_empty() {
            let _ = writeln!(err, "ERROR: {} not found", matches[0].display());
            rc = 1;
            continue;
        }
        found.extend(existing);
    }
    (dedupe(&found), rc)
}

fn parse_targets(spec: &str, chips: &[Chip]) -> std::result::Result<Vec<usize>, String> {
    let lowered = spec.trim().to_lowercase();
    if lowered.is_empty() || lowered == "all" || lowered == "*" {
        return Ok((0..chips.len()).collect());
    }
    let mut out: Vec<usize> = Vec::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() || token.to_lowercase() == "all" {
            out.extend(0..chips.len());
            continue;
        }
        if !token.is_empty() && token.chars().all(|ch| ch.is_ascii_digit()) {
            let Ok(number) = token.parse::<i64>() else {
                return Err(format!(
                    "chip {token} is out of range (1..{})",
                    chips.len()
                ));
            };
            let idx = number - 1;
            if idx < 0 || idx >= chips.len() as i64 {
                return Err(format!(
                    "chip {token} is out of range (1..{})",
                    chips.len()
                ));
            }
            out.push(idx as usize);
            continue;
        }
        let lower = token.to_lowercase();
        let matches: Vec<usize> = chips
            .iter()
            .filter(|chip| chip.name().to_lowercase().contains(&lower))
            .map(|chip| chip.index)
            .collect();
        if matches.is_empty() {
            return Err(format!("no chip matching '{token}'"));
        }
        out.extend(matches);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn sample_counts(info: &ConvertInfo) -> String {
    let mut samples = format!("{} PCM", info.pcm_samples);
    let pairs: [(i64, &str); 24] = [
        (info.segapcm_samples as i64, "SEGA"),
        (info.rf5c68_samples as i64, "RF5C68"),
        (info.rf5c164_samples as i64, "RF5C164"),
        (info.c140_samples as i64, "C140"),
        (info.c219_samples as i64, "C219"),
        (info.c352_samples as i64, "C352"),
        (info.k053260_samples as i64, "K053260"),
        (info.k054539_samples as i64, "K054539"),
        (info.ymz280b_samples as i64, "YMZ280B"),
        (info.multipcm_samples as i64, "MultiPCM"),
        (info.upd7759_samples as i64, "uPD7759"),
        (info.pwm_samples as i64, "PWM"),
        (info.msm5205_samples as i64, "MSM5205"),
        (info.ga20_samples as i64, "GA20"),
        (i64::from(info.msm5232_notes), "MSM5232"),
        (info.mikey_samples as i64, "Mikey DAC"),
        (info.x1_010_samples as i64, "X1-010 PCM"),
        (info.qsound_samples as i64, "QSound"),
        (info.wonderswan_pcm_samples as i64, "WS PCM"),
        (info.oki_samples as i64, "OKI"),
        (info.msm6258_samples as i64, "ADPCM"),
        (info.ym2610_a_samples as i64, "ADPCM-A"),
        (info.ym2610_b_samples as i64, "ADPCM-B"),
        (info.y8950_b_samples as i64, "Y8950 ADPCM"),
    ];
    for (count, label) in pairs {
        if count != 0 {
            samples.push_str(&format!("  {count} {label}"));
        }
    }
    samples
}

type Group = (Vec<PathBuf>, PathBuf);

fn merge_groups(groups: Vec<Group>) -> Vec<Group> {
    let mut merged: Vec<(String, Vec<PathBuf>)> = Vec::new();
    let mut order: Vec<PathBuf> = Vec::new();
    for (files, dest) in groups {
        let key = dest.to_string_lossy().to_lowercase();
        let slot = match merged.iter().position(|(k, _)| *k == key) {
            Some(index) => index,
            None => {
                merged.push((key, Vec::new()));
                order.push(dest);
                merged.len() - 1
            }
        };
        let have: Vec<String> = merged[slot]
            .1
            .iter()
            .map(|path| path.to_string_lossy().to_lowercase())
            .collect();
        for path in files {
            let marker = path.to_string_lossy().to_lowercase();
            if !have.contains(&marker) {
                merged[slot].1.push(path);
            }
        }
    }
    order
        .into_iter()
        .map(|dest| {
            let key = dest.to_string_lossy().to_lowercase();
            let files = merged
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, files)| files.clone())
                .unwrap_or_default();
            (files, dest)
        })
        .collect()
}

fn min_row_for_bpm(max_bpm: Option<f64>) -> Option<f64> {
    match max_bpm {
        Some(bpm) if bpm > 0.0 => Some(15.0 * 44100.0 / bpm),
        _ => None,
    }
}

fn collect_furs(tokens: &[String], err: &mut dyn Write) -> (Vec<PathBuf>, i32) {
    let (paths, mut rc) = resolve(tokens, err);
    if paths.is_empty() {
        return (Vec::new(), if rc != 0 { rc } else { 1 });
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for path in &paths {
        let hits = fur_targets(path);
        if hits.is_empty() {
            let _ = writeln!(err, "ERROR: no .fur under {}", path.display());
            rc = 1;
            continue;
        }
        found.extend(hits);
    }
    (dedupe(&found), rc)
}

fn peek(tokens: &[String]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for token in tokens {
        found.extend(expand_token(token).into_iter().filter(|p| p.exists()));
    }
    dedupe(&found)
}

fn has_vgm(path: &Path) -> bool {
    let (direct, packs) = vgm_targets(path, None);
    !direct.is_empty() || !packs.is_empty()
}

fn suffix_is_vgm(path: &Path) -> bool {
    let suffix = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    VGM_SUFFIXES.contains(&suffix.as_str())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_convert(
    args: &Parsed,
    out: &mut dyn Write,
    err: &mut dyn Write,
    driver: &dyn CliDriver,
) -> Result<i32> {
    let (paths, mut rc) = resolve(&args.input, err);
    if paths.is_empty() {
        return Ok(if rc != 0 { rc } else { 1 });
    }
    let out_root: Option<PathBuf> = args
        .out
        .as_deref()
        .filter(|text| !text.is_empty())
        .map(|text| PathBuf::from(clean_text(text)));
    let (groups, group_rc) = convert_groups(
        &paths,
        out_root.as_deref(),
        out,
        err,
    )?;
    rc = if rc != 0 { rc } else { group_rc };
    if groups.is_empty() {
        return Ok(if rc != 0 { rc } else { 1 });
    }
    let pcm = !args.melody_only;
    let include_fm = !args.pcm_only;
    let mut factor_list: Option<Vec<f64>> = None;
    if let Some(factors) = args.factors.as_deref().filter(|text| !text.is_empty()) {
        let mut parsed: Vec<f64> = Vec::new();
        for part in factors.split(',') {
            if part.trim().is_empty() {
                continue;
            }
            let Some(number) = parse_float(part) else {
                let _ = writeln!(err, "ERROR: --factors takes a comma list of numbers");
                return Ok(2);
            };
            parsed.push(number);
        }
        factor_list = Some(parsed);
    }
    let use_variants = args.variants
        || args.optimize.as_deref().unwrap_or("none") != "none"
        || args.condense.is_some()
        || factor_list.is_some()
        || args.verify
        || args.dirs;
    let optimize = match args.optimize.as_deref() {
        Some(value) if value != "none" => value.to_string(),
        _ if args.variants || args.verify => "lossless".to_string(),
        _ => "all".to_string(),
    };
    let condense = match args.condense {
        Some(0) | None => 4,
        Some(value) => value,
    };
    let opts = ConvertOptions {
        speed: args.speed,
        tick_hz: None,
        pcm,
        tick_samples: None,
        min_row: min_row_for_bpm(args.max_bpm),
        variants: use_variants,
        condense: f64::from(condense),
        factors: factor_list,
        optimize,
        include_fm,
        normalize: !args.no_normalize,
        loop_find: !args.no_loop_find,
        verify: args.verify,
        layout: if args.dirs { "dirs" } else { "flat" }.to_string(),
        c352_quad: args.c352_quad,
        lfo_loop: !args.no_lfo_loop,
        lossy_samples: args.lossy,
        workers: args.workers.map(|w| w.max(1) as usize),
    };
    let mut grand_done = 0usize;
    let mut grand_total = 0usize;
    let group_count = groups.len();
    for (files, dest) in groups {
        let mut done = 0usize;
        let count = files.len();
        {
            let out_cell = std::cell::RefCell::new(&mut *out);
            let err_cell = std::cell::RefCell::new(&mut *err);
            let mut log = |line: &str| {
                let _ = writeln!(*out_cell.borrow_mut(), "{line}");
            };
            let mut on_row = |row: ConvertRow| {
                if let Some(error) = row.error {
                    let _ = writeln!(
                        *err_cell.borrow_mut(),
                        "ERROR {}: {}",
                        name_of(&row.src),
                        error
                    );
                    rc = 1;
                    return;
                }
                let Some(info) = row.info else { return };
                done += 1;
                if let Some(variants) = info.variants.as_deref().filter(|v| !v.is_empty()) {
                    let _ = writeln!(
                        *out_cell.borrow_mut(),
                        "{} -> {} variants",
                        name_of(&row.src),
                        variants.len()
                    );
                    for line in format_variant_lines(variants, &dest) {
                        let _ = writeln!(*out_cell.borrow_mut(), "    {line}");
                    }
                    return;
                }
                let mut extra = String::new();
                let notes: Vec<&str> = info
                    .warnings
                    .iter()
                    .map(String::as_str)
                    .filter(|warning| {
                        info.loop_.as_deref() != Some(*warning)
                    })
                    .collect();
                if !notes.is_empty() {
                    extra = format!("  ({})", notes[..notes.len().min(4)].join("; "));
                }
                let vol = match info.gain {
                    Some(gain) => format!("  vol {gain:.2}x"),
                    None => String::new(),
                };
                let loop_text = match info.loop_.as_deref() {
                    Some(note) if !note.is_empty() => format!("  {note}"),
                    _ => String::new(),
                };
                let _ = writeln!(
                    *out_cell.borrow_mut(),
                    "{} -> {}  {} bytes  speed {}  {} FM ins  {}  {} notes{}{}{}",
                    name_of(&row.src),
                    info.dst,
                    info.bytes,
                    info.speed,
                    info.fm_patches,
                    sample_counts(&info),
                    info.events,
                    vol,
                    loop_text,
                    extra
                );
            };
            driver.convert_folder(&files, &dest, &opts, &mut log, &mut on_row)?;
        }
        let _ = writeln!(out, "{done}/{count} converted to {}", dest.display());
        grand_done += done;
        grand_total += count;
    }
    if group_count > 1 {
        let _ = writeln!(out, "{grand_done}/{grand_total} converted");
    }
    Ok(rc)
}

fn convert_groups(
    paths: &[PathBuf],
    out_root: Option<&Path>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(Vec<Group>, i32)> {
    let mut groups: Vec<Group> = Vec::new();
    let mut collections: Vec<Group> = Vec::new();
    let mut rc = 0;
    for path in paths {
        let (direct, packs) = vgm_targets(path, None);
        if !direct.is_empty() {
            let dest = match out_root {
                Some(root) => root.to_path_buf(),
                None => {
                    let base = if path.is_file() {
                        path.parent().map(Path::to_path_buf).unwrap_or_default()
                    } else {
                        path.clone()
                    };
                    base.join("fur")
                }
            };
            groups.push((direct, dest));
            continue;
        }
        if packs.is_empty() {
            let _ = writeln!(err, "ERROR: no .vgm/.vgz under {}", path.display());
            rc = 1;
            continue;
        }
        let many = packs.len() > 1;
        for pack in &packs {
            let (tracks, _playlist) = crate::packs::pack_tracks(pack)?;
            if tracks.is_empty() {
                continue;
            }
            let dest = match out_root {
                None => pack.path.join("fur"),
                Some(root) if many => root.join(pack.name()),
                Some(root) => root.to_path_buf(),
            };
            collections.push((tracks, dest));
        }
    }
    if collections.len() > 1 {
        let total: usize = collections.iter().map(|(tracks, _)| tracks.len()).sum();
        let _ = writeln!(out, "{} packs, {} tracks", collections.len(), total);
    }
    groups.extend(collections);
    Ok((merge_groups(groups), rc))
}

fn cmd_report(args: &Parsed, out: &mut dyn Write, err: &mut dyn Write, driver: &dyn CliDriver) -> Result<i32> {
    let (paths, mut rc) = resolve(&args.input, err);
    if paths.is_empty() {
        return Ok(if rc != 0 { rc } else { 1 });
    }
    let mut wrote = false;
    let mut direct_all: Vec<PathBuf> = Vec::new();
    for path in &paths {
        let (direct, packs) = vgm_targets(path, None);
        if !direct.is_empty() {
            direct_all.extend(direct);
            continue;
        }
        if !packs.is_empty() {
            let _ = writeln!(out, "{} packs in {}", packs.len(), path.display());
            for pack in &packs {
                let label = {
                    let chip_label = pack.chip_label();
                    if chip_label.is_empty() {
                        String::new()
                    } else {
                        format!("  {chip_label}")
                    }
                };
                let _ = writeln!(
                    out,
                    "  {:4}  {}{}",
                    pack.tracks.len(),
                    pack.name(),
                    label
                );
            }
            let _ = writeln!(out, "pass a pack folder to report every track");
            wrote = true;
            continue;
        }
        let _ = writeln!(err, "ERROR: no .vgm/.vgz under {}", path.display());
        rc = 1;
    }
    if !direct_all.is_empty() {
        let opts = ReportOptions {
            speed: args.speed,
            pcm: true,
            tick_samples: None,
            min_row: min_row_for_bpm(args.max_bpm),
            workers: args.workers.map(|w| w.max(1) as usize),
        };
        let mut log = |line: &str| {
            let _ = writeln!(out, "{line}");
        };
        for (src, text, error) in driver.report_files(&dedupe(&direct_all), &opts, &mut log) {
            if let Some(error) = error {
                let _ = writeln!(err, "ERROR {}: {}", name_of(&src), error);
                rc = 1;
                continue;
            }
            if let Some(text) = text {
                let _ = out.write_all(text.as_bytes());
                if !text.ends_with('\n') {
                    let _ = out.write_all(b"\n");
                }
            }
        }
        wrote = true;
    }
    if !wrote && rc == 0 {
        return Ok(1);
    }
    Ok(rc)
}

fn cmd_edit(args: &Parsed, out: &mut dyn Write, err: &mut dyn Write, driver: &dyn CliDriver) -> Result<i32> {
    let (files, mut rc) = collect_furs(&args.input, err);
    if files.is_empty() {
        return Ok(if rc != 0 { rc } else { 1 });
    }
    if (args.factor.is_none()) == (args.value.is_none()) && !args.list {
        let _ = writeln!(err, "ERROR: give --factor F or --value V (or --list)");
        return Ok(2);
    }
    if files.len() > 1 {
        let _ = writeln!(out, "{} files", files.len());
    }
    let chip_spec = args.chip.clone().unwrap_or_else(|| "all".to_string());
    for fur in &files {
        let chips = match driver.read_chips(fur) {
            Ok(chips) => chips,
            Err(exc) => {
                let _ = writeln!(err, "ERROR {}: {exc}", name_of(fur));
                rc = 1;
                continue;
            }
        };
        let targets: Vec<usize> = if args.list {
            Vec::new()
        } else {
            match parse_targets(&chip_spec, &chips) {
                Ok(targets) => targets,
                Err(message) => {
                    let _ = writeln!(err, "{message}");
                    return Ok(1);
                }
            }
        };
        let chips = if !args.list {
            match driver.set_chip_volume(fur, args.factor, args.value, Some(&targets)) {
                Ok(_) => match driver.read_chips(fur) {
                    Ok(chips) => chips,
                    Err(exc) => {
                        let _ = writeln!(err, "ERROR {}: {exc}", name_of(fur));
                        rc = 1;
                        continue;
                    }
                },
                Err(exc) => {
                    let _ = writeln!(err, "ERROR {}: {exc}", name_of(fur));
                    rc = 1;
                    continue;
                }
            }
        } else {
            chips
        };
        for chip in &chips {
            let mark = if targets.contains(&chip.index) { "*" } else { " " };
            let _ = writeln!(
                out,
                "{} {}  {}. {}  {:.3}",
                mark,
                name_of(fur),
                chip.index + 1,
                chip.name(),
                chip.volume
            );
        }
    }
    Ok(rc)
}

fn value_i64(value: Option<&serde_json::Value>) -> i64 {
    value.and_then(serde_json::Value::as_i64).unwrap_or(0)
}

fn cmd_compare(args: &Parsed, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let source = PathBuf::from(clean_text(&args.source));
    let fur = PathBuf::from(clean_text(&args.fur));
    if !source.is_file() {
        let _ = writeln!(err, "ERROR: {} not found", source.display());
        return Ok(1);
    }
    if !fur.is_file() {
        let _ = writeln!(err, "ERROR: {} not found", fur.display());
        return Ok(1);
    }
    let channel = args.channel.unwrap_or(0);
    let (runs, exported) = match crate::diag::compare_fur_to_source(
        &source,
        &fur,
        channel,
        0.5,
        100.0,
        None,
    ) {
        Ok(result) => result,
        Err(exc) => {
            let _ = writeln!(err, "ERROR: {exc}");
            return Ok(1);
        }
    };
    let src_vgm = crate::vgm::load_vgm(&source)?;
    let exp_vgm = crate::vgm::load_vgm(&exported)?;
    let mut field_bad: i64 = 0;
    let chips: std::collections::HashSet<&str> =
        src_vgm.writes.iter().map(|w| w.chip).collect();
    if chips.contains("ym2610") || chips.contains("ym2610_ssg") {
        let samples = crate::audit::compare_ym2610_samples(&src_vgm, &exp_vgm);
        for kind in ["a", "b"] {
            let Some(d) = samples.get(kind) else { continue };
            let matched = value_i64(d.get("matched"));
            let missed = value_i64(d.get("missed"));
            if matched + missed == 0 {
                continue;
            }
            let _ = writeln!(
                out,
                "ADPCM-{}: {}/{} matched, data {}, length {}, vol {}, pan {}, rate {}",
                kind.to_uppercase(),
                matched,
                matched + missed,
                value_i64(d.get("data")),
                value_i64(d.get("length")),
                value_i64(d.get("vol")),
                value_i64(d.get("pan")),
                value_i64(d.get("rate"))
            );
            field_bad += ["data", "length", "vol", "pan", "rate"]
                .iter()
                .map(|key| value_i64(d.get(*key)))
                .sum::<i64>();
        }
    }
    if chips.contains("ym2608") || chips.contains("ym2608_ssg") {
        let samples = crate::audit::compare_ym2608_samples(&src_vgm, &exp_vgm);
        for (label, kind) in [("rhythm", "rhythm"), ("delta-T", "delta_t")] {
            let Some(d) = samples.get(kind) else { continue };
            let matched = value_i64(d.get("matched"));
            let missed = value_i64(d.get("missed"));
            if matched + missed == 0 {
                continue;
            }
            let _ = writeln!(
                out,
                "{}: {}/{} matched, extra {}, data {}, length {}, vol {}, pan {}, rate {}",
                label,
                matched,
                matched + missed,
                value_i64(d.get("extra")),
                value_i64(d.get("data")),
                value_i64(d.get("length")),
                value_i64(d.get("vol")),
                value_i64(d.get("pan")),
                value_i64(d.get("rate"))
            );
            field_bad += ["data", "length", "vol", "pan", "rate"]
                .iter()
                .map(|key| value_i64(d.get(*key)))
                .sum::<i64>();
        }
    }
    if runs.is_empty() {
        let _ = writeln!(
            out,
            "channel {channel}: no sustained divergence above 0.5 st"
        );
        return Ok(if field_bad != 0 { 1 } else { 0 });
    }
    let _ = writeln!(out, "channel {channel}: {} run(s) of divergence", runs.len());
    for run in &runs {
        let _ = writeln!(out, "  {run}");
    }
    Ok(1)
}

fn cmd_render(args: &Parsed, out: &mut dyn Write, err: &mut dyn Write, driver: &dyn CliDriver) -> Result<i32> {
    let (files, mut rc) = collect_furs(&args.input, err);
    if files.is_empty() {
        return Ok(if rc != 0 { rc } else { 1 });
    }
    let out_arg = args.out.as_deref().filter(|text| !text.is_empty());
    if out_arg.is_some() && files.len() > 1 {
        let _ = writeln!(err, "ERROR: -o takes a single .fur input");
        return Ok(2);
    }
    let loops = args.loops.unwrap_or(1) as i64;
    for fur in &files {
        let out_path = match out_arg {
            Some(text) => PathBuf::from(clean_text(text)),
            None => fur.with_extension("wav"),
        };
        if let Err(exc) = driver.render_wav(fur, &out_path, loops) {
            let _ = writeln!(err, "ERROR {}: {exc}", name_of(fur));
            rc = 1;
            continue;
        }
        let size = std::fs::metadata(&out_path).map(|meta| meta.len()).unwrap_or(0);
        let _ = writeln!(
            out,
            "{} -> {}  ({} bytes)",
            name_of(fur),
            out_path.display(),
            size
        );
    }
    Ok(rc)
}

fn compare_paths(paths: &[PathBuf], channel: i32, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let mut source: Option<&PathBuf> = None;
    let mut fur: Option<&PathBuf> = None;
    for path in paths {
        if !path.is_file() {
            continue;
        }
        if suffix_is_vgm(path) && source.is_none() {
            source = Some(path);
        } else if path.extension().is_some_and(|ext| ext.to_string_lossy().to_lowercase() == "fur")
            && fur.is_none()
        {
            fur = Some(path);
        }
    }
    let (Some(source), Some(fur)) = (source, fur) else {
        let _ = writeln!(err, "ERROR: compare needs one .vgm/.vgz and one .fur");
        return Ok(2);
    };
    let args = Parsed {
        source: source.to_string_lossy().into_owned(),
        fur: fur.to_string_lossy().into_owned(),
        channel: Some(channel),
        ..Parsed::default()
    };
    cmd_compare(&args, out, err)
}

fn run_paths(argv: &[String], out: &mut dyn Write, err: &mut dyn Write, driver: &dyn CliDriver) -> Result<i32> {
    let mut parsed = match parse_args(&BARE_SPECS, argv) {
        ParseOutcome::Args(parsed) => parsed,
        ParseOutcome::Help => {
            write_out(out, BARE_USAGE);
            return Ok(0);
        }
        ParseOutcome::Error(message) => {
            parser_error(err, BARE_USAGE, "vgm2151fur", &message);
            return Ok(2);
        }
    };
    if let Some(message) = required(&parsed, 1, "PATH") {
        parser_error(err, BARE_USAGE, "vgm2151fur", &message);
        return Ok(2);
    }
    if !parsed.unknown.is_empty() {
        parser_error(
            err,
            BARE_USAGE,
            "vgm2151fur",
            &format!("unrecognized arguments: {}", parsed.unknown.join(" ")),
        );
        return Ok(2);
    }
    let wants_edit = parsed.list || parsed.factor.is_some() || parsed.value.is_some();
    let wants_render = parsed.loops.is_some();
    let wants_compare = parsed.channel.is_some();
    let existing = peek(&parsed.positionals);
    let has_vgm_flag = existing.iter().any(|path| has_vgm(path));
    let has_fur = existing.iter().any(|path| !fur_targets(path).is_empty());
    if wants_compare {
        return compare_paths(&existing, parsed.channel.unwrap_or(0), out, err);
    }
    parsed.input = parsed.positionals.clone();
    if wants_edit || (has_fur && !has_vgm_flag && !wants_render) {
        if parsed.chip.is_none() {
            parsed.chip = Some("all".to_string());
        }
        if parsed.factor.is_none() && parsed.value.is_none() {
            parsed.list = true;
        }
        return cmd_edit(&parsed, out, err, driver);
    }
    if wants_render {
        if parsed.loops.is_none() {
            parsed.loops = Some(1);
        }
        return cmd_render(&parsed, out, err, driver);
    }
    cmd_convert(&parsed, out, err, driver)
}

fn menu_state_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    match home {
        Some(home) => home.join(".vgm2151fur.json"),
        None => PathBuf::from(".vgm2151fur.json"),
    }
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn run_menu(out: &mut dyn Write) -> Result<i32> {
    use crate::menu::ReadResult;
    let mut read = || -> ReadResult {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => ReadResult::Eof,
            Ok(_) => {
                if line.ends_with('\n') {
                    line.pop();
                }
                if line.ends_with('\r') {
                    line.pop();
                }
                ReadResult::Line(line)
            }
            Err(exc) if exc.kind() == std::io::ErrorKind::Interrupted => ReadResult::Interrupt,
            Err(_) => ReadResult::Eof,
        }
    };
    crate::menu::run_menu(&mut read, out, &menu_state_path(), &crate::menu::RealDriver)
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

pub fn run(
    args: Vec<String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    driver: &dyn CliDriver,
) -> Result<i32> {
    if driver.interrupt() {
        return Err(Error::Interrupt);
    }
    main_impl(args, out, err, driver)
}

fn main_impl(
    args_in: Vec<String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    driver: &dyn CliDriver,
) -> Result<i32> {
    if args_in.is_empty() || args_in == ["menu".to_string()] {
        if args_in == ["menu".to_string()] || interactive() {
            return run_menu(out);
        }
        write_out(out, MAIN_HELP);
        return Ok(0);
    }
    let head = &args_in[0];
    if !COMMANDS.contains(&head.as_str()) && !head.starts_with('-') {
        return run_paths(&args_in, out, err, driver);
    }

    // Top-level options (argparse's main parser).
    let mut index = 0;
    let mut command: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    while index < args_in.len() {
        let token = &args_in[index];
        if token == "-h" || token == "--help" {
            write_out(out, MAIN_HELP);
            return Ok(0);
        }
        if token == "--version" {
            let _ = writeln!(out, "vgm2151fur {}", crate::CONVERTER_VERSION);
            return Ok(0);
        }
        if let Some(inline) = token.strip_prefix("--help=") {
            parser_error(
                err,
                MAIN_USAGE,
                "vgm2151fur",
                &format!("argument -h/--help: ignored explicit argument '{inline}'"),
            );
            return Ok(2);
        }
        if let Some(inline) = token.strip_prefix("--version=") {
            parser_error(
                err,
                MAIN_USAGE,
                "vgm2151fur",
                &format!("argument --version: ignored explicit argument '{inline}'"),
            );
            return Ok(2);
        }
        if token == "--" {
            index += 1;
            break;
        }
        if token.starts_with('-') && token != "-" && !looks_negative_number(token) {
            parser_error(
                err,
                MAIN_USAGE,
                "vgm2151fur",
                &format!("unrecognized arguments: {token}"),
            );
            return Ok(2);
        }
        break;
    }
    if index < args_in.len() {
        command = Some(args_in[index].clone());
        rest = args_in[index + 1..].to_vec();
    }
    let Some(command) = command else {
        if interactive() {
            return run_menu(out);
        }
        write_out(out, MAIN_HELP);
        return Ok(0);
    };
    if !COMMANDS.contains(&command.as_str()) {
        parser_error(
            err,
            MAIN_USAGE,
            "vgm2151fur",
            &format!(
                "argument {{convert,report,edit,compare,render,menu}}: invalid choice: '{command}' (choose from 'convert', 'report', 'edit', 'compare', 'render', 'menu')"
            ),
        );
        return Ok(2);
    }
    if command == "menu" {
        let outcome = parse_args(&MENU_SPECS, &rest);
        match outcome {
            ParseOutcome::Help => {
                write_out(out, MENU_HELP);
                return Ok(0);
            }
            ParseOutcome::Args(parsed) => {
                if !parsed.positionals.is_empty() || !parsed.unknown.is_empty() {
                    let mut extras = parsed.positionals.clone();
                    extras.extend(parsed.unknown.iter().cloned());
                    parser_error(
                        err,
                        MAIN_USAGE,
                        "vgm2151fur",
                        &format!("unrecognized arguments: {}", extras.join(" ")),
                    );
                    return Ok(2);
                }
            }
            ParseOutcome::Error(message) => {
                parser_error(err, MENU_USAGE, "vgm2151fur menu", &message);
                return Ok(2);
            }
        }
        if interactive() {
            return run_menu(out);
        }
        write_out(out, MAIN_HELP);
        return Ok(0);
    }

    let (usage, help, prog, specs, min_positionals, metavar): (
        &str,
        &str,
        &str,
        &[Spec],
        usize,
        &str,
    ) = match command.as_str() {
        "convert" => (
            CONVERT_USAGE,
            CONVERT_HELP,
            "vgm2151fur convert",
            &CONVERT_SPECS,
            1,
            "PATH",
        ),
        "report" => (
            REPORT_USAGE,
            REPORT_HELP,
            "vgm2151fur report",
            &REPORT_SPECS,
            1,
            "PATH",
        ),
        "edit" => (
            EDIT_USAGE,
            EDIT_HELP,
            "vgm2151fur edit",
            &EDIT_SPECS,
            1,
            "PATH",
        ),
        "compare" => (
            COMPARE_USAGE,
            COMPARE_HELP,
            "vgm2151fur compare",
            &COMPARE_SPECS,
            2,
            "source, fur",
        ),
        _ => (
            RENDER_USAGE,
            RENDER_HELP,
            "vgm2151fur render",
            &RENDER_SPECS,
            1,
            "PATH",
        ),
    };
    let mut parsed = match parse_args(specs, &rest) {
        ParseOutcome::Args(parsed) => parsed,
        ParseOutcome::Help => {
            write_out(out, help);
            return Ok(0);
        }
        ParseOutcome::Error(message) => {
            parser_error(err, usage, prog, &message);
            return Ok(2);
        }
    };
    if command == "compare" {
        if parsed.positionals.len() < 2 {
            let missing = if parsed.positionals.is_empty() {
                "source, fur"
            } else {
                "fur"
            };
            parser_error(
                err,
                usage,
                prog,
                &format!("the following arguments are required: {missing}"),
            );
            return Ok(2);
        }
        if parsed.positionals.len() > 2 || !parsed.unknown.is_empty() {
            let mut extras = parsed.positionals[2..].to_vec();
            extras.extend(parsed.unknown.iter().cloned());
            parser_error(
                err,
                MAIN_USAGE,
                "vgm2151fur",
                &format!("unrecognized arguments: {}", extras.join(" ")),
            );
            return Ok(2);
        }
        parsed.source = parsed.positionals[0].clone();
        parsed.fur = parsed.positionals[1].clone();
        return cmd_compare(&parsed, out, err);
    }
    if let Some(message) = required(&parsed, min_positionals, metavar) {
        parser_error(err, usage, prog, &message);
        return Ok(2);
    }
    if !parsed.unknown.is_empty() {
        parser_error(
            err,
            MAIN_USAGE,
            "vgm2151fur",
            &format!("unrecognized arguments: {}", parsed.unknown.join(" ")),
        );
        return Ok(2);
    }
    parsed.input = parsed.positionals.clone();
    match command.as_str() {
        "convert" => cmd_convert(&parsed, out, err, driver),
        "report" => cmd_report(&parsed, out, err, driver),
        "edit" => cmd_edit(&parsed, out, err, driver),
        _ => cmd_render(&parsed, out, err, driver),
    }
}

/// `main`: wire stdio, run, and map the control-flow errors to exit codes.
pub fn main_entry() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let driver = RealDriver;
    let code = match run(args, &mut out, &mut err, &driver) {
        Ok(code) => code,
        Err(Error::Interrupt) => {
            let _ = err.write_all(b"cancelled (Ctrl+C)\n");
            130
        }
        Err(Error::Exit(code)) => code,
        Err(exc) => {
            let _ = writeln!(err, "ERROR: {exc}");
            1
        }
    };
    let _ = out.flush();
    let _ = err.flush();
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        rows: RefCell<Vec<ConvertRow>>,
        chips: RefCell<Vec<Chip>>,
        interrupt: bool,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                rows: RefCell::new(Vec::new()),
                chips: RefCell::new(vec![Chip {
                    index: 0,
                    id: 0x82,
                    volume: 1.0,
                    pan: 0.0,
                    pan_fr: 0.0,
                }]),
                interrupt: false,
            }
        }
    }

    impl CliDriver for Fake {
        fn convert_folder(
            &self,
            _files: &[PathBuf],
            _dest: &Path,
            _opts: &ConvertOptions,
            _log: &mut dyn FnMut(&str),
            on_row: &mut dyn FnMut(ConvertRow),
        ) -> Result<()> {
            for row in self.rows.borrow().iter() {
                on_row(row.clone());
            }
            Ok(())
        }

        fn report_files(
            &self,
            files: &[PathBuf],
            _opts: &ReportOptions,
            _log: &mut dyn FnMut(&str),
        ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
            files
                .iter()
                .map(|f| (f.clone(), Some("report text".to_string()), None))
                .collect()
        }

        fn render_wav(&self, _fur: &Path, out: &Path, _loops: i64) -> Result<PathBuf> {
            Ok(out.to_path_buf())
        }

        fn read_chips(&self, _fur: &Path) -> Result<Vec<Chip>> {
            Ok(self.chips.borrow().clone())
        }

        fn set_chip_volume(
            &self,
            _fur: &Path,
            factor: Option<f64>,
            value: Option<f64>,
            targets: Option<&[usize]>,
        ) -> Result<Vec<Chip>> {
            let mut chips = self.chips.borrow_mut();
            for chip in chips.iter_mut() {
                let chosen = targets.is_none_or(|t| t.contains(&chip.index));
                if !chosen {
                    continue;
                }
                chip.volume = match value {
                    Some(value) => value,
                    None => chip.volume * factor.unwrap_or(1.0),
                };
            }
            Ok(chips.clone())
        }

        fn interrupt(&self) -> bool {
            self.interrupt
        }
    }

    fn run_with(args: &[&str], fake: &Fake) -> (i32, String, String) {
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let code = run(
            args.iter().map(|s| s.to_string()).collect(),
            &mut out,
            &mut err,
            fake,
        )
        .map_err(|exc| panic!("unexpected error: {exc}"))
        .unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn version_prints_the_reference_tag() {
        let fake = Fake::new();
        let (code, out, err) = run_with(&["--version"], &fake);
        assert_eq!(code, 0);
        assert_eq!(out, "vgm2151fur 0.11.0\n");
        assert_eq!(err, "");
    }

    #[test]
    fn missing_input_reports_an_error_and_code_one() {
        let fake = Fake::new();
        let (code, out, err) = run_with(&["report", "no-such-file.vgm"], &fake);
        assert_eq!(code, 1);
        assert_eq!(out, "");
        assert_eq!(err, "ERROR: no-such-file.vgm not found\n");
    }

    #[test]
    fn unknown_flag_is_an_argparse_style_error() {
        let fake = Fake::new();
        let (code, _out, err) = run_with(&["convert", "x.vgm", "--bogus"], &fake);
        assert_eq!(code, 2);
        assert!(err.starts_with("usage: vgm2151fur [-h] [--version]"), "{err}");
        assert!(
            err.ends_with("vgm2151fur: error: unrecognized arguments: --bogus\n"),
            "{err}"
        );
    }

    #[test]
    fn edit_needs_a_change() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let fur = dir.path().join("Song.fur");
        fs::write(&fur, b"x").unwrap();
        let fake = Fake::new();
        let (code, _out, err) =
            run_with(&["edit", fur.to_str().unwrap()], &fake);
        assert_eq!(code, 2);
        assert_eq!(
            err,
            "ERROR: give --factor F or --value V (or --list)\n"
        );
    }

    #[test]
    fn bare_fur_path_lists_chips() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let fur = dir.path().join("Song.fur");
        fs::write(&fur, b"x").unwrap();
        let fake = Fake::new();
        let (code, out, err) = run_with(&[fur.to_str().unwrap()], &fake);
        assert_eq!(code, 0);
        assert_eq!(err, "");
        assert!(out.contains("Song.fur  1. YM2151  1.000"), "{out}");
    }

    #[test]
    fn interrupt_is_reported_as_the_error() {
        let mut fake = Fake::new();
        fake.interrupt = true;
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let result = run(vec!["convert".to_string()], &mut out, &mut err, &fake);
        assert!(matches!(result, Err(Error::Interrupt)));
    }
}

// ---------------------------------------------------------------------------
// Test seam
// ---------------------------------------------------------------------------

/// The parsed flags the reference tests read off `build_parser().parse_args`.
#[derive(Clone, Debug, Default)]
pub struct ParsedArgs {
    pub no_lfo_loop: bool,
    pub lossy_sample_clustering: bool,
    pub no_normalize: bool,
    pub verify: bool,
    pub variants: bool,
    pub speed: Option<i32>,
    pub tick_hz: Option<f64>,
}

/// `build_parser().parse_args(argv)` for the assertions the reference tests
/// make. `argv[0]` selects the subcommand's option table exactly like
/// argparse; a bare path uses the top-level table.
pub fn parse_for_test(argv: &[String]) -> Result<ParsedArgs> {
    let command = argv.first().map(|s| s.as_str()).unwrap_or("");
    let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
    let specs: &[Spec] = match command {
        "convert" => &CONVERT_SPECS,
        "report" => &REPORT_SPECS,
        "edit" => &EDIT_SPECS,
        "compare" => &COMPARE_SPECS,
        "render" => &RENDER_SPECS,
        "menu" => &MENU_SPECS,
        _ => &BARE_SPECS,
    };
    let tokens: Vec<String> = if COMMANDS.contains(&command) {
        rest.clone()
    } else {
        argv.to_vec()
    };
    match parse_args(specs, &tokens) {
        ParseOutcome::Args(parsed) => {
            let out = ParsedArgs {
                no_lfo_loop: parsed.no_lfo_loop,
                lossy_sample_clustering: parsed.lossy,
                no_normalize: parsed.no_normalize,
                verify: parsed.verify,
                variants: parsed.variants,
                speed: parsed.speed,
                tick_hz: None,
            };
            Ok(out)
        }
        ParseOutcome::Help => Err(Error::Exit(0)),
        ParseOutcome::Error(message) => Err(Error::Msg(message)),
    }
}
