// SPDX-License-Identifier: GPL-2.0-only

//! `stg export-forest` implementation.

use anyhow::Result;
use clap::Arg;

use crate::{
    argset,
    branchloc::BranchLocator,
    ext::RepositoryExtended,
    nl_extensions::{apply_branch_forest_plan, inquire_confirm, plan_branch_forest, preview_branch_forest_plan, GitNLExtensions},
    patch::PatchLocator,
    stack::{InitializationPolicy, Stack, StackStateAccess},
};

pub(super) const STGIT_COMMAND: super::StGitCommand = super::StGitCommand {
    name: "export-forest",
    category: super::CommandCategory::StackInspection,
    make,
    run,
};

fn make() -> clap::Command {
    clap::Command::new(STGIT_COMMAND.name)
        .about("Export patches as a forest of Git branches")
        .long_about(
            "Export the current stack as a hierarchical forest of Git branches where \
             each patch becomes a separate branch. The branches are configured to \
             establish a pull hierarchy that mirrors the stack's patch order.\n\
             \n\
             Each patch in the stack becomes a branch named '{prefix}/{patchname}'. \
             The first patch branch is configured to pull from the current branch's \
             upstream, and each subsequent branch pulls from the previous one.\n\
             \n\
             Use --clean to delete any existing branches with the same prefix before \
             creating new ones. With --force, deletion happens without confirmation.\n\
             \n\
             This is useful for creating a branch-based workflow from a patch stack \
             while preserving the hierarchical relationships between patches.",
        )
        .arg(
            Arg::new("prefix")
                .long("prefix")
                .short('p')
                .help("Namespace prefix for created branches")
                .long_help(
                    "Namespace prefix for all created branches. Each patch will become \
                     a branch named '{prefix}/{patchname}'. The prefix cannot be empty."
                )
                .value_name("prefix")
                .required(true),
        )
        .arg(
            Arg::new("leaf")
                .long("leaf")
                .short('l')
                .help("Patch to start the forest from")
                .long_help(
                    "Optional patch to start the forest from. If specified, only creates \
                     branches from this patch to the top of the stack. If not specified, \
                     creates branches for all applied patches in the stack."
                )
                .value_name("patch")
                .value_parser(clap::value_parser!(PatchLocator)),
        )
        .arg(
            Arg::new("clean")
                .long("clean")
                .short('c')
                .help("Delete existing branches under prefix before creating new ones")
                .long_help(
                    "Delete any existing branches that match the prefix pattern before \
                     creating new branches. If existing branches are found, will ask for \
                     interactive confirmation unless --force is specified."
                )
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .short('f')
                .help("Force deletion without confirmation when --clean is used")
                .long_help(
                    "When used with --clean, automatically delete existing branches \
                     without asking for confirmation."
                )
                .requires("clean")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .short('n')
                .help("Show what would be done without making any changes")
                .long_help(
                    "Preview the branch forest operation without actually creating \
                     any branches or modifying configurations. Shows all planned \
                     operations and any conflicts that would prevent execution."
                )
                .action(clap::ArgAction::SetTrue),
        )
        .arg(argset::branch_arg())
}

fn run(matches: &clap::ArgMatches) -> Result<()> {
    let repo = gix::Repository::open()?;
    let opt_branch = matches.get_one::<BranchLocator>("branch");
    let stack = Stack::from_branch_locator(
        &repo,
        opt_branch,
        InitializationPolicy::RequireInitialized,
    )?;

    // Check that we have applied patches to export
    if stack.applied().is_empty() {
        return Err(super::Error::NoAppliedPatches.into());
    }

    let prefix = matches
        .get_one::<String>("prefix")
        .expect("required argument");
    let clean_flag = matches.get_flag("clean");
    let force_flag = matches.get_flag("force");
    let dry_run = matches.get_flag("dry-run");

    // Handle cleaning existing branches if requested
    if clean_flag && !dry_run {
        let existing_branches = repo.iter_branches_with_prefix(prefix)?;
        if !existing_branches.is_empty() {
            println!("Found {} existing branch(es) with prefix '{}':", existing_branches.len(), prefix);
            for branch in &existing_branches {
                println!("  {}", branch);
            }
            
            let should_delete = if force_flag {
                true
            } else {
                inquire_confirm(&format!("Delete {} existing branch(es)?", existing_branches.len()))?
            };
            
            if should_delete {
                delete_branches(&repo, &existing_branches)?;
                println!("Deleted {} branch(es)", existing_branches.len());
            } else {
                println!("Aborted - existing branches not deleted");
                return Ok(());
            }
        }
    }

    // Resolve the optional leaf patch
    let start_patch = if let Some(patch_locator) = matches.get_one::<PatchLocator>("leaf") {
        let patch_name = patch_locator.resolve_name(&stack)?;
        Some(patch_name)
    } else {
        None
    };

    // Plan the operation
    let plan = plan_branch_forest(&repo, &stack, prefix, start_patch.as_ref())?;

    // Show preview of planned operations
    preview_branch_forest_plan(&plan, clean_flag)?;

    // For dry-run, just show the plan and exit
    if dry_run {
        println!("\n[DRY RUN] No changes made.");
        return Ok(());
    }

    // If no branches to create, exit early
    if plan.branches.is_empty() {
        return Ok(());
    }

    // Ask for confirmation before applying
    if !force_flag {
        println!();
        if !inquire_confirm(&format!("Create {} branch(es)?", plan.branches.len()))? {
            println!("Operation cancelled.");
            return Ok(());
        }
    }

    // Apply the plan
    apply_branch_forest_plan(&repo, &plan)?;

    println!("Branch forest exported successfully");
    Ok(())
}

/// Delete the specified branches.
fn delete_branches(repo: &gix::Repository, branch_names: &[String]) -> Result<()> {
    for branch_name in branch_names {
        let branch_refname = format!("refs/heads/{}", branch_name);
        if let Ok(reference) = repo.find_reference(&branch_refname) {
            reference.delete()?;
        }
    }
    Ok(())
}