use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;

use anyhow::anyhow;
use anyhow::Result;
use bstr::ByteSlice;
use inquire::ui::RenderConfig;
use inquire::ui::Styled;
use rand::Rng;

use crate::ext::CommitExtended;
use crate::patch::PatchName;
use crate::stack::Stack;
use crate::stack::StackAccess;
use crate::stack::StackStateAccess;
use crate::stupid::Stupid;

const DEFAULT_PATCH_PREFIX: &str = "misc";
const DEFAULT_RANDOM_ID_LENGTH: usize = 5;

// ----------------------------------------------------------------------------
// Patch Helpers
// ----------------------------------------------------------------------------

pub(crate) fn generate_patch_id(stack: &Stack, proposed_name: Option<String>, interactive: bool) -> Result<PatchName> {
    let patch_prefix = if let Some(prefix) = proposed_name {
        // Use the explicitly provided prefix
        prefix
    } else {
        // Attempt to determine prefix from stack, or use default
        patch_find_last_used_prefix(stack)
            .unwrap_or_else(|| DEFAULT_PATCH_PREFIX.to_string())
    };

    let patch_prefix_selected = if interactive {
        // Ask user for prefix using inquire, use patch name as a default value
        inquire_ask("Pick patch prefix:", Some(patch_prefix.as_str()))?
    } else {
        // Use auto-determined prefix without prompting
        patch_prefix
    };

    let random_id_suffix = patch_generate_id(DEFAULT_RANDOM_ID_LENGTH);

    patch_generate_name_with_suffix(&patch_prefix_selected, &random_id_suffix)
}

pub(crate) fn validate_refresh_intentions(
    repo: &gix::Repository,
    stack: &Stack,
    target_patch: &PatchName,
    temp_commit: Rc<gix::Commit>,
) -> Result<()> {
    let stupid = repo.stupid();

    let target_patch_name = target_patch.to_string();
    let target_patch_commit = stack.get_patch_commit(target_patch);
    let target_patch_tree_id = target_patch_commit.tree_id()?.detach();
    let target_patch_parent_commit = target_patch_commit.get_parent_commit()?;
    let target_patch_parent_tree_id = target_patch_parent_commit.tree_id()?.detach();

    let temp_commit_tree_id = temp_commit.tree_id()?.detach();
    let temp_commit_parent_tree_id = temp_commit.get_parent_commit()?.tree_id()?.detach();

    let target_patch_stack_commit = repo
        .find_reference(stack.get_stack_refname())?
        .peel_to_commit()?;
    let target_patch_description_raw = target_patch_commit.message()?.title.to_string();
    let target_patch_description = target_patch_description_raw.trim_end();

    let mut diff_output_old = stupid.diff_tree_files_status(
        /* tree1 */ target_patch_parent_tree_id,
        /* tree2 */ target_patch_tree_id,
        /* stat */ true,
        /* name_only */ false,
        /* use_color */ true,
    )?;
    if diff_output_old.is_empty() {
        diff_output_old = "[No changes in the patch]".to_string().into();
    }

    let mut diff_output_new = stupid.diff_tree_files_status(
        /* tree1 */ temp_commit_parent_tree_id,
        /* tree2 */ temp_commit_tree_id,
        /* stat */ true,
        /* name_only */ false,
        /* use_color */ true,
    )?;
    if diff_output_new.is_empty() {
        diff_output_new = "[No changes in the patch]".to_string().into();
    }

    // Print diff output
    println!(":: Checking intentions for patch: {}", target_patch_name);
    println!();
    println!(":> Patch SHA   : {}", target_patch_commit.id);
    println!(":> Stack SHA   : {}", target_patch_stack_commit.id);
    println!();
    println!(":> Patch Subject");
    println!(
        "{}",
        bstring_prepend_lines(
            &target_patch_description.as_bytes().as_bstr().to_owned(),
            "\t".to_string()
        )
    );
    println!();
    println!(":> Old Patch:");
    println!(
        "{}",
        bstring_prepend_lines(&diff_output_old, "\t".to_string())
    );
    println!();
    println!(":> New Changes:");
    println!(
        "{}",
        bstring_prepend_lines(&diff_output_new, "\t".to_string())
    );
    println!();

    if inquire_confirm("Show Diff?")? {
        println!(
            ":! git diff {} {}",
            temp_commit_parent_tree_id, temp_commit_tree_id
        );
        stupid
            .git_cmd()
            .args(["diff"])
            .args([
                temp_commit_parent_tree_id.to_string(),
                temp_commit_tree_id.to_string(),
            ])
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .stdin(std::process::Stdio::inherit())
            .spawn()?
            .wait()?;
    }

    if !inquire_confirm("Refresh patch?")? {
        return Err(anyhow!("refresh operation aborted"));
    }

    Ok(())
}


// ----------------------------------------------------------------------------
// Git Helpers / Extensions
// ----------------------------------------------------------------------------

pub trait GitNLExtensions {
    fn git_data_file(&self, path: &str) -> PathBuf;
}

impl GitNLExtensions for gix::Repository {
    fn git_data_file(&self, path: &str) -> PathBuf {
        // If STG_EDIT_IN_CWD is set return path as is.
        match std::env::var("STG_EDIT_IN_CWD") {
            Ok(_) => PathBuf::from(path),
            Err(_) => self.path().join(path),
        }
    }
}

// ----------------------------------------------------------------------------
// Patch Helpers
// ----------------------------------------------------------------------------

pub(crate) fn patch_generate_id(length: usize) -> String {
    const RND_CHARSET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    let alphabet_dist = rand::distr::slice::Choose::new(RND_CHARSET).unwrap();

    rand::rng()
        .sample_iter(alphabet_dist)
        .take(length)
        .map(|c| *c as char)
        .collect()
}

fn patch_parse_prefix_from_patch_name(patch_name: String) -> Option<String> {
    if let Some(at_pos) = patch_name.find("@") {
        let (patch_prefix, _) = patch_name.split_at(at_pos);
        Some(patch_prefix.to_string())
    } else {
        None
    }
}

pub(crate) fn patch_generate_name_with_suffix(prefix: &str, suffix: &str) -> Result<PatchName> {
    if prefix.is_empty() {
        return Err(anyhow!("patch prefix cannot be empty"));
    }

    Ok(PatchName::from_str(&format!("{}@{}", prefix, suffix))?)
}

pub(crate) fn patch_find_last_used_prefix(stack: &Stack) -> Option<String> {
    stack.applied().last().and_then(|p| {
        patch_parse_prefix_from_patch_name(p.to_string())
    })
}

// ----------------------------------------------------------------------------
// String Helpers
// ----------------------------------------------------------------------------

fn bstring_prepend_lines(str: &bstr::BString, prefix: String) -> bstr::BString {
    str.lines()
        .map(|line| bstr::concat([prefix.as_bytes(), line]).as_bstr().to_owned())
        .collect::<Vec<bstr::BString>>()
        .join(bstr::B("\n"))
        .into()
}

// ----------------------------------------------------------------------------
// Inquire Helper Functions
// ----------------------------------------------------------------------------

fn inquire_default_render_config<'a>() -> RenderConfig<'a> {
    let cfg = if atty::is(atty::Stream::Stdout) {
        RenderConfig::default()
    } else {
        RenderConfig::empty()
    };
    cfg.with_prompt_prefix(Styled::new(":?").with_style_sheet(cfg.prompt_prefix.style))
        .with_answered_prompt_prefix(
            Styled::new(":>").with_style_sheet(cfg.answered_prompt_prefix.style),
        )
}

fn inquire_confirm(prompt: &str) -> Result<bool> {
    let res = inquire::Confirm::new(prompt)
        .with_render_config(inquire_default_render_config())
        .prompt()?;
    Ok(res)
}

fn inquire_ask(prompt: &str, default: Option<&str>) -> Result<String> {
    if atty::is(atty::Stream::Stdout) {
        let res = inquire::Text::new(prompt)
            .with_initial_value(default.unwrap_or_default())
            .with_render_config(inquire_default_render_config())
            .prompt()?;
        Ok(res)
    } else {
        default
            .map(str::to_string)
            .ok_or(anyhow!("no default provided"))
    }
}