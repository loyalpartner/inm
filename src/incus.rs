//! The one place that knows how to talk to the `incus` CLI.
//!
//! Both the instance list and the console tunnel go through here so the binary
//! path, the argument shapes, and the "names are only unique per project" rule
//! live in a single file.

use gpui::SharedString;
use serde::Deserialize;
use std::process::Command;

pub const BIN: &str = "/opt/homebrew/bin/incus";

/// An instance is identified by (project, name): names are only unique within
/// a project, so everything downstream — tabs, console lookups — keys on both.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct VmId {
    pub project: SharedString,
    pub name: SharedString,
}

#[derive(Clone)]
pub struct Vm {
    pub id: VmId,
    pub status: SharedString,
}

impl Vm {
    pub fn running(&self) -> bool {
        self.status.as_ref() == "Running"
    }
}

#[derive(Deserialize)]
struct InstanceRaw {
    name: String,
    status: String,
    project: String,
    #[serde(rename = "type")]
    kind: String,
}

/// Every virtual machine across every project, sorted by project then name.
pub fn list_vms() -> Vec<Vm> {
    let Ok(output) = Command::new(BIN)
        .args(["list", "--all-projects", "--format", "json"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let raws: Vec<InstanceRaw> = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let mut vms: Vec<Vm> = raws
        .into_iter()
        .filter(|v| v.kind == "virtual-machine")
        .map(|v| Vm {
            id: VmId {
                project: v.project.into(),
                name: v.name.into(),
            },
            status: v.status.into(),
        })
        .collect();
    vms.sort_by(|a, b| {
        a.id.project
            .cmp(&b.id.project)
            .then_with(|| a.id.name.cmp(&b.id.name))
    });
    vms
}

pub fn start(id: &VmId) -> Result<(), String> {
    run(&["start", &id.name, "--project", &id.project])
}

fn run(args: &[&str]) -> Result<(), String> {
    match Command::new(BIN).args(args).output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}
