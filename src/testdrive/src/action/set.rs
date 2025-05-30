// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::cmp;

use anyhow::{Context, bail};
use regex::Regex;
use tokio::fs;

use crate::action::{ControlFlow, State};
use crate::parser::BuiltinCommand;

pub const DEFAULT_REGEX_REPLACEMENT: &str = "<regex_match>";

pub fn run_regex_set(
    mut cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    let regex: Regex = cmd.args.parse("match")?;
    let replacement = cmd
        .args
        .opt_string("replacement")
        .unwrap_or_else(|| DEFAULT_REGEX_REPLACEMENT.into());
    cmd.args.done()?;

    state.regex = Some(regex);
    state.regex_replacement = replacement;
    Ok(ControlFlow::Continue)
}

pub fn run_regex_unset(
    cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    cmd.args.done()?;
    state.regex = None;
    state.regex_replacement = DEFAULT_REGEX_REPLACEMENT.to_string();
    Ok(ControlFlow::Continue)
}

pub fn run_sql_timeout(
    mut cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    let duration = cmd.args.string("duration")?;
    let duration = if duration.to_lowercase() == "default" {
        None
    } else {
        Some(humantime::parse_duration(&duration).context("parsing duration")?)
    };
    let force = cmd.args.opt_bool("force")?.unwrap_or(false);
    cmd.args.done()?;
    state.timeout = duration.unwrap_or(state.default_timeout);
    if !force {
        // Bump the timeout to be at least the default timeout unless the
        // timeout has been forced.
        state.timeout = cmp::max(state.timeout, state.default_timeout);
    }
    Ok(ControlFlow::Continue)
}

pub fn run_max_tries(
    mut cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    let max_tries = cmd.args.string("max-tries")?;
    cmd.args.done()?;
    state.max_tries = max_tries.parse::<usize>()?;
    Ok(ControlFlow::Continue)
}

pub fn run_set_arg_default(
    cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    for (key, val) in cmd.args {
        let arg_key = format!("arg.{key}");
        state.cmd_vars.entry(arg_key).or_insert(val);
    }

    Ok(ControlFlow::Continue)
}

pub fn set_vars(cmd: BuiltinCommand, state: &mut State) -> Result<ControlFlow, anyhow::Error> {
    for (key, val) in cmd.args {
        if val.is_empty() {
            state.cmd_vars.insert(key, cmd.input.join("\n"));
        } else {
            state.cmd_vars.insert(key, val);
        }
    }

    Ok(ControlFlow::Continue)
}

pub async fn run_set_from_sql(
    mut cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    let var = cmd.args.string("var")?;
    cmd.args.done()?;

    let row = state
        .materialize
        .pgclient
        .query_one(&cmd.input.join("\n"), &[])
        .await
        .context("running query")?;
    if row.columns().len() != 1 {
        bail!(
            "set-from-sql query must return exactly one column, but it returned {}",
            row.columns().len()
        );
    }
    let value: String = row.try_get(0).context("deserializing value as string")?;

    state.cmd_vars.insert(var, value);

    Ok(ControlFlow::Continue)
}

pub async fn run_set_from_file(
    cmd: BuiltinCommand,
    state: &mut State,
) -> Result<ControlFlow, anyhow::Error> {
    for (key, path) in cmd.args {
        println!("Setting {} to contents of {}...", key, path);
        let contents = fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {path}"))?;
        state.cmd_vars.insert(key, contents);
    }
    Ok(ControlFlow::Continue)
}
