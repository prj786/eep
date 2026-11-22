//! The one place Nix evaluation happens: once per release, on the builder.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::util::{sha256_hex, write_atomic};

const EVAL_NIX: &str = include_str!("../eval.nix");
const IMAGE: &str = "docker.io/nixos/nix";
const VOLUME: &str = "eep-nix";
const EXPR: &str = "import /work/eval.nix (builtins.fromJSON (builtins.readFile /work/args.json))";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// `nix` on this machine (CI runners).
    Native,
    /// The nixos/nix image, with a named volume so tarballs download once.
    Podman,
}

impl Runner {
    pub fn pick(choice: &str) -> Result<Self> {
        Ok(match choice {
            "native" => Self::Native,
            "podman" => Self::Podman,
            "auto" if on_path("nix") => Self::Native,
            "auto" if on_path("podman") => Self::Podman,
            "auto" => bail!("need either nix or podman"),
            other => bail!("unknown --nix {other:?} (auto, native, podman)"),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalApp {
    pub error: Option<String>,
    pub out: Option<String>,
    #[serde(default)]
    pub pname: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub main_program: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub homepage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalOut {
    pub apps: BTreeMap<String, EvalApp>,
    pub driver: Vec<String>,
}

/// Evaluate `attrs` in the release at `url`. Cached by (eval.nix, url, attrs).
pub fn eval(runner: Runner, url: &str, attrs: &[String], cache: &Path) -> Result<EvalOut> {
    let args = serde_json::json!({ "url": url, "attrs": attrs });
    let args = serde_json::to_vec(&args)?;
    let key = sha256_hex(&[EVAL_NIX.as_bytes(), &args].concat());
    let cached = cache.join("eval").join(format!("{}.json", &key[..32]));
    if let Ok(b) = std::fs::read(&cached) {
        return serde_json::from_slice(&b).context("cached eval");
    }

    let work = cache.join(format!("work-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    std::fs::write(work.join("eval.nix"), EVAL_NIX)?;
    std::fs::write(work.join("args.json"), &args)?;
    let nix_args = [
        "eval",
        "--json",
        "--impure",
        "--extra-experimental-features",
        "nix-command",
        "--expr",
    ];
    let output = match runner {
        Runner::Native => Command::new("nix")
            .args(nix_args)
            .arg(EXPR.replace("/work/", &format!("{}/", work.display())))
            .stderr(Stdio::inherit())
            .output(),
        Runner::Podman => Command::new("podman")
            .args(["run", "--rm", "-v", &format!("{VOLUME}:/root/.cache/nix")])
            .args(["-v", &format!("{}:/work:ro", work.display()), IMAGE, "nix"])
            .args(nix_args)
            .arg(EXPR)
            .stderr(Stdio::inherit())
            .output(),
    };
    let _ = std::fs::remove_dir_all(&work);
    let output = output.context("running nix eval")?;
    if !output.status.success() {
        bail!("nix eval failed ({})", output.status);
    }
    let out: EvalOut = serde_json::from_slice(&output.stdout).context("nix eval output")?;
    write_atomic(&cached, &output.stdout)?;
    Ok(out)
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
}
