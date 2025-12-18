#[cfg(test)]
extern crate assert_cmd;
extern crate predicates;
use assert_cmd::prelude::*;
use std::process::Command;

#[test]
fn test_cli_no_args() {
    let mut cmd = Command::cargo_bin("sgc").expect("Binary not found");
    cmd.assert().failure();
}

#[test]
fn test_version() {
    let mut cmd = Command::cargo_bin("sgc").expect("Binary not found");
    cmd.arg("--version").assert().success();
}

#[test]
fn test_help() {
    let mut cmd = Command::cargo_bin("sgc").expect("Binary not found");
    cmd.arg("--help").assert().success();
}
