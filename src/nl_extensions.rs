use std::str::FromStr;

use bstr::io::BufReadExt;
use bstr::BStr;
use bstr::B;
use bstr::ByteSlice;
use rand::Rng;
use anyhow::anyhow;
use anyhow::Result;

use crate::ext::CommitExtended;
use crate::patch::PatchName;
use crate::stack::Stack;
use crate::stack::StackAccess;
use crate::stack::StackStateAccess;
use crate::stupid::Stupid;

// TODO(nonlogical): Make this configurable via git config.
const DEFAULT_PATCH_PREFIX: &str = "misc";
const DEFAULT_PATCH_ID_CHARSET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

pub(crate) fn generate_and_edit_patch_id(stack: &Stack) -> Result<PatchName> {
    let patch_prefix = 
        // Attempt to parse patch prefix from the last applied patch.
        stack.applied().last().map(|p| { 
            parse_patch_prefix_from_patch_name(p.to_string())
        })
        .unwrap_or(None)
        // Alternatively use `DEFAULT_PATCH_PREFIX`.
        .unwrap_or_else(|| { DEFAULT_PATCH_PREFIX.to_string()});

    // Ask user for prefix using inquire, use patch name as a default value
    let patch_prefix_selected = inquire::Text::new("Pick patch prefix")
        .with_default(patch_prefix.as_str())
        .prompt()?;

    if patch_prefix_selected.is_empty() {
        return Err(anyhow!("patch prefix cannot be empty"));
    }

    // Generate a random string of numbers + lowercase letters.
    let alphabet_dist = rand::distr::slice::Choose::new(DEFAULT_PATCH_ID_CHARSET).unwrap();

    let random_id_suffix = rand::rng()
        .sample_iter(alphabet_dist)
        .take(5)
        .map(|c| *c as char)
        .collect::<String>();

    Ok(PatchName::from_str(&format!("{}@{}", patch_prefix_selected, random_id_suffix))?)
}

pub(crate) fn validate_refresh_intentions<'repo>(
    repo: &gix::Repository, 
    stack: &Stack,
    target_patch: &PatchName,
    new_tree_id: gix::ObjectId,
) -> Result<()> {
    let stupid = repo.stupid();

    let target_patch_name = target_patch.to_string();
    let target_patch_commit = stack.get_patch_commit(target_patch);
    let target_patch_tree_id = target_patch_commit.tree_id()?.detach();
    let target_patch_parent_commit  = target_patch_commit.get_parent_commit()?;
    let target_patch_parent_tree_id = target_patch_parent_commit.tree_id()?.detach();

    let target_patch_stack_commit = repo.find_reference(stack.get_stack_refname())?.peel_to_commit()?;
    let target_patch_description_raw = target_patch_commit.message()?.title.to_string();
    let target_patch_description = target_patch_description_raw.trim_ascii_end();

    let mut diff_output_old = stupid.diff_tree_files_status(
        /* tree1 */  target_patch_parent_tree_id,
        /* tree2 */  target_patch_tree_id,
        /* stat */    true,
        /* name_only */ false,
        /* use_color */ true,
    )?;
    if diff_output_old.is_empty() {
        diff_output_old = "[No changes in the patch]".to_string().into();
    }

    let mut diff_output_new = stupid.diff_tree_files_status(
        /* tree1 */  target_patch_tree_id,
        /* tree2 */  new_tree_id,
        /* stat */    true,
        /* name_only */ false,
        /* use_color */ true,
    )?;
    if diff_output_new.is_empty() {
        diff_output_new = "[No changes in the patch]".to_string().into();
    }

    // Print diff output
    println!(":: Checking intentions for patch: {}", target_patch_name);
    println!("");
    println!(":> Patch SHA   : {}", target_patch_commit.id);
    println!(":> Stack SHA   : {}", target_patch_stack_commit.id);
    println!("");
    println!(":> Patch Subject");
    println!("");
    println!("{}", bstring_prepend_lines(&target_patch_description.as_bytes().as_bstr().to_owned(), "\t".to_string()));
    println!("");
    println!(":: Old Patch:");
    println!("");
    println!("{}", bstring_prepend_lines(&diff_output_old, "\t".to_string()));
    println!("");
    println!(":: New Changes:");
    println!("");
    println!("{}", bstring_prepend_lines(&diff_output_new, "\t".to_string()));
    println!("");

    let should_show_diff = inquire::Confirm::new("Show diff?")
        .prompt()?;

    if should_show_diff {
        println!(":: CMD: git diff {} {}", target_patch_parent_tree_id, target_patch_tree_id);
        stupid.git_cmd()
            .args(["diff"])
            .args([target_patch_tree_id.to_string(), new_tree_id.to_string()])
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .stdin(std::process::Stdio::inherit())
            .spawn()?
            .wait()?;
    }

    let should_refresh = inquire::Confirm::new("Are you sure you want to refresh this patch?")
        .prompt()?;

    if !should_refresh {
        return Err(anyhow!("user aborted"));
    }

    Ok(())
}


fn parse_patch_prefix_from_patch_name(patch_name: String) -> Option<String> {
    if let Some(at_pos) = patch_name.find("@") {
        let (patch_prefix, _) = patch_name.split_at(at_pos);
        Some(patch_prefix.to_string())
    } else {
        None
    }
}

fn bstring_prepend_lines(str: &bstr::BString, prefix: String) -> bstr::BString {
    str
        .lines()
        .map(|line| bstr::concat([prefix.as_bytes(), line]).as_bstr().to_owned())
        .collect::<Vec<bstr::BString>>()
        .join(bstr::B("\n"))
        .into()
}