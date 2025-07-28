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
// Branch Forest Helpers
// ----------------------------------------------------------------------------

/// Configuration for a single branch in the forest export operation.
#[derive(Debug, Clone)]
pub(crate) struct BranchForestBranchConfig {
    /// Name of the branch to create (e.g., "dev/feature1")
    pub branch_name: String,
    /// Full reference name (e.g., "refs/heads/dev/feature1") 
    pub branch_refname: String,
    /// Commit ID that the branch should point to
    pub commit_id: gix::ObjectId,
    /// Name of the patch this branch represents
    pub patch_name: String,
    /// Remote name for upstream configuration (e.g., "origin" or ".")
    pub upstream_remote: Option<String>,
    /// Merge reference for upstream configuration (e.g., "refs/heads/main")
    pub upstream_merge: Option<String>,
}

/// Complete plan for branch forest export operation.
#[derive(Debug)]
pub(crate) struct BranchForestPlan {
    /// List of branch configurations to create
    pub branches: Vec<BranchForestBranchConfig>,
    /// List of existing branches that would conflict
    pub conflicts: Vec<String>,
    /// Any validation errors found during planning
    pub validation_errors: Vec<String>,
}

impl BranchForestPlan {
    /// Check if the plan has any conflicts or errors that would prevent execution
    pub fn has_issues(&self) -> bool {
        !self.conflicts.is_empty() || !self.validation_errors.is_empty()
    }
    
    /// Get a summary of issues for display to user
    pub fn get_issues_summary(&self) -> Vec<String> {
        let mut issues = Vec::new();
        
        for conflict in &self.conflicts {
            issues.push(format!("Conflict: Branch '{}' already exists", conflict));
        }
        
        for error in &self.validation_errors {
            issues.push(format!("Error: {}", error));
        }
        
        issues
    }
}

/// Export the current stack as a forest of Git branches.
///
/// This function creates a hierarchical branch structure from the current stack where:
/// - Each patch in the stack becomes a separate branch named `{prefix}/{patchname}`
/// - The first patch branch is configured to pull from the current branch
/// - Each subsequent patch branch is configured to pull from the previous patch branch
/// - The branch hierarchy mirrors the stack's patch order and dependencies
///
/// # Parameters
/// - `repo`: The Git repository containing the stack
/// - `stack`: The current stack with applied patches
/// - `prefix`: The namespace prefix for all created branches
/// - `start_patch`: Optional patch to start the forest from. If `None`, exports all applied patches
///
/// # Behavior
/// - Creates branches without modifying any commits or the stack itself, or working tree
/// - Sets up branch configuration to establish the pull hierarchy
/// - Preserves the original stack state and commits
/// - Each branch points to the exact commit of its corresponding patch
/// - When `start_patch` is specified, only creates branches from that patch to the top of the stack
///
/// # Example
/// For a stack with patches `base`, `feature1`, `feature2`, `feature3` and prefix `dev`:
/// - With `start_patch = None`: Creates all 4 branches (`dev/base`, `dev/feature1`, `dev/feature2`, `dev/feature3`)
/// - With `start_patch = Some("feature1")`: Creates branches `dev/feature1`, `dev/feature2`, `dev/feature3`
/// - The first branch in the subset pulls from the current branch where export was initiated
/// - Each subsequent branch pulls from the previous one in the subset
///
/// # Returns
/// - `Ok(())` on successful branch forest creation
/// - `Err` if any branch creation or configuration fails, or if `start_patch` is not found
pub(crate) fn export_branch_forest(repo: &gix::Repository, stack: &Stack, prefix: &str, start_patch: Option<&PatchName>) -> Result<()> {
    // Plan the operation
    let plan = plan_branch_forest(repo, stack, prefix, start_patch)?;
    
    // Check for issues that would prevent execution
    if plan.has_issues() {
        let issues = plan.get_issues_summary();
        return Err(anyhow!("cannot export branch forest: {}", issues.join("; ")));
    }
    
    // Apply the plan
    apply_branch_forest_plan(repo, &plan)?;
    
    Ok(())
}

/// Plan the creation of a branch forest from the current stack.
///
/// This function analyzes the current stack and generates a complete plan for creating
/// a hierarchical branch structure. It performs all validation and conflict detection
/// without making any changes to the repository.
///
/// # Parameters
/// - `repo`: The Git repository containing the stack
/// - `stack`: The current stack with applied patches
/// - `prefix`: The namespace prefix for all created branches
/// - `start_patch`: Optional patch to start the forest from. If `None`, exports all applied patches
///
/// # Returns
/// - `Ok(BranchForestPlan)` containing all planned operations and any conflicts/errors found
/// - `Err` only for serious errors like invalid inputs or repository access issues
pub(crate) fn plan_branch_forest(
    repo: &gix::Repository,
    stack: &Stack,
    prefix: &str,
    start_patch: Option<&PatchName>,
) -> Result<BranchForestPlan> {
    let mut plan = BranchForestPlan {
        branches: Vec::new(),
        conflicts: Vec::new(),
        validation_errors: Vec::new(),
    };

    if prefix.is_empty() {
        plan.validation_errors.push("prefix cannot be empty".to_string());
        return Ok(plan);
    }

    let applied_patches = stack.applied();
    if applied_patches.is_empty() {
        plan.validation_errors.push("no patches to export".to_string());
        return Ok(plan);
    }

    // Determine start index based on start_patch parameter
    let start_index = if let Some(start_patch) = start_patch {
        if !stack.has_patch(start_patch) {
            plan.validation_errors.push(format!("patch '{}' not found in applied patches", start_patch));
            return Ok(plan);
        }
        
        match applied_patches.iter().position(|p| p == start_patch) {
            Some(index) => index,
            None => {
                plan.validation_errors.push(format!("patch '{}' is not in applied patches", start_patch));
                return Ok(plan);
            }
        }
    } else {
        // Default to exporting all patches if no start_patch specified
        0
    };

    // Create patch subset from start_index to end
    let patches_to_export = &applied_patches[start_index..];
    if patches_to_export.is_empty() {
        plan.validation_errors.push("no patches to export from specified start".to_string());
        return Ok(plan);
    }

    let current_branch_name = stack.get_branch_name();
    
    let config = repo.config_snapshot();
    let current_upstream_remote = config
        .string_by("branch", Some(current_branch_name.into()), "remote")
        .map(|v| v.as_bstr().to_string());
    let current_upstream_merge = config
        .string_by("branch", Some(current_branch_name.into()), "merge")
        .map(|v| v.as_bstr().to_string());

    let mut previous_branch_name: Option<String> = None;

    for (index, patch_name) in patches_to_export.iter().enumerate() {
        let patch_commit = stack.get_patch_commit(patch_name);
        let branch_name = format!("{}/{}", prefix, patch_name);
        let branch_refname = format!("refs/heads/{}", branch_name);

        // Check for conflicts with existing branches
        if repo.try_find_reference(&branch_refname)?.is_some() {
            plan.conflicts.push(branch_name.clone());
        }

        // Validate branch name
        if let Err(e) = gix::refs::FullName::try_from(branch_refname.as_str()) {
            plan.validation_errors.push(format!("invalid branch name '{}': {}", branch_name, e));
            continue;
        }

        // Determine upstream configuration
        let (upstream_remote, upstream_merge) = if index == 0 {
            // First branch (deepest in stack) tracks the current branch we're exporting from
            (Some(".".to_string()), Some(format!("refs/heads/{}", current_branch_name)))
        } else if let Some(prev_branch) = &previous_branch_name {
            // Subsequent branches pull from previous branch
            (Some(".".to_string()), Some(format!("refs/heads/{}", prev_branch)))
        } else {
            (None, None)
        };

        plan.branches.push(BranchForestBranchConfig {
            branch_name: branch_name.clone(),
            branch_refname,
            commit_id: patch_commit.id,
            patch_name: patch_name.to_string(),
            upstream_remote,
            upstream_merge,
        });

        previous_branch_name = Some(branch_name);
    }

    Ok(plan)
}

/// Apply a branch forest plan by creating all planned branches and configurations.
///
/// This function executes all the operations specified in a `BranchForestPlan`,
/// creating branches and setting up their upstream configurations. It assumes
/// the plan has been validated and any conflicts resolved.
///
/// # Parameters
/// - `repo`: The Git repository to modify
/// - `plan`: The validated plan containing all operations to execute
///
/// # Returns
/// - `Ok(())` on successful execution of all operations
/// - `Err` if any branch creation or configuration fails
pub(crate) fn apply_branch_forest_plan(repo: &gix::Repository, plan: &BranchForestPlan) -> Result<()> {
    use crate::ext::RepositoryExtended;

    if plan.has_issues() {
        return Err(anyhow::anyhow!("cannot apply plan with conflicts or validation errors"));
    }

    for branch_config in &plan.branches {
        // Create the branch reference
        repo.edit_reference(gix::refs::transaction::RefEdit {
            change: gix::refs::transaction::Change::Update {
                log: gix::refs::transaction::LogChange {
                    mode: gix::refs::transaction::RefLog::AndReference,
                    force_create_reflog: false,
                    message: format!("export_branch_forest: create branch {}", branch_config.branch_name).into(),
                },
                expected: gix::refs::transaction::PreviousValue::MustNotExist,
                new: gix::refs::Target::Object(branch_config.commit_id),
            },
            name: gix::refs::FullName::try_from(branch_config.branch_refname.as_str())
                .map_err(|e| anyhow::anyhow!("invalid branch name '{}': {}", branch_config.branch_name, e))?,
            deref: false,
        })?;

        // Configure upstream if specified
        if let (Some(remote), Some(merge)) = (&branch_config.upstream_remote, &branch_config.upstream_merge) {
            let mut config_file = repo.local_config_file()?;
            
            config_file.set_raw_value_by(
                "branch",
                Some(branch_config.branch_name.as_str().into()),
                "remote",
                remote.as_bytes(),
            )?;
            config_file.set_raw_value_by(
                "branch",
                Some(branch_config.branch_name.as_str().into()),
                "merge",
                merge.as_bytes(),
            )?;
            
            repo.write_local_config(config_file)?;
        }
    }

    Ok(())
}

/// Preview a branch forest plan by displaying planned operations and any issues.
///
/// This function shows a formatted preview of all planned branch operations,
/// including any conflicts or validation errors. It provides a clear summary
/// of what would be done during execution.
///
/// # Parameters
/// - `plan`: The branch forest plan to preview
/// - `clean_flag`: Whether the --clean flag is active (affects suggestion text)
///
/// # Returns
/// - `Ok(())` if the plan can be executed
/// - `Err` if the plan has conflicts or validation errors that prevent execution
pub(crate) fn preview_branch_forest_plan(plan: &BranchForestPlan, clean_flag: bool) -> Result<()> {
    println!("Branch Forest Plan:");
    println!("==================");
    
    if plan.branches.is_empty() {
        println!("No branches to create.");
        return Ok(());
    }

    for (i, branch_config) in plan.branches.iter().enumerate() {
        println!(
            "{}. Create branch '{}' -> {}",
            i + 1,
            branch_config.branch_name,
            &branch_config.commit_id.to_string()[..8]
        );
        
        if let (Some(remote), Some(merge)) = (&branch_config.upstream_remote, &branch_config.upstream_merge) {
            println!("   Upstream: {} -> {}", remote, merge);
        }
    }

    // Show any issues found
    if plan.has_issues() {
        println!("\nIssues found:");
        for issue in plan.get_issues_summary() {
            println!("  ! {}", issue);
        }
        
        if !clean_flag && !plan.conflicts.is_empty() {
            println!("\nUse --clean to delete existing branches automatically.");
        }
        
        return Err(anyhow!("Cannot proceed due to conflicts or errors"));
    }

    Ok(())
}

// ----------------------------------------------------------------------------
// Git Helpers / Extensions
// ----------------------------------------------------------------------------

pub trait GitNLExtensions {
    fn git_data_file(&self, path: &str) -> PathBuf;
    fn iter_branches_with_prefix(&self, prefix: &str) -> Result<Vec<String>>;
}

impl GitNLExtensions for gix::Repository {
    fn git_data_file(&self, path: &str) -> PathBuf {
        // If STG_EDIT_IN_CWD is set return path as is.
        match std::env::var("STG_EDIT_IN_CWD") {
            Ok(_) => PathBuf::from(path),
            Err(_) => self.path().join(path),
        }
    }

    fn iter_branches_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let prefix_pattern = format!("refs/heads/{}/", prefix);
        let mut matching_branches = Vec::new();

        for reference in self.references()?.all()?.filter_map(Result::ok) {
            if let Some(name) = reference.name().as_bstr().to_str().ok() {
                if name.starts_with(&prefix_pattern) {
                    // Extract just the branch name (without refs/heads/)
                    if let Some(branch_name) = name.strip_prefix("refs/heads/") {
                        matching_branches.push(branch_name.to_string());
                    }
                }
            }
        }

        matching_branches.sort();
        Ok(matching_branches)
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

pub(crate) fn inquire_default_render_config<'a>() -> RenderConfig<'a> {
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

pub(crate) fn inquire_confirm(prompt: &str) -> Result<bool> {
    let res = inquire::Confirm::new(prompt)
        .with_render_config(inquire_default_render_config())
        .prompt()?;
    Ok(res)
}

pub(crate) fn inquire_ask(prompt: &str, default: Option<&str>) -> Result<String> {
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