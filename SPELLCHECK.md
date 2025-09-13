# Spellcheck Implementation

This document describes the systematic spellcheck implementation for the Agave project.

## Overview

We use the [typos](https://github.com/crate-ci/typos) tool to automatically check spelling in comments and markdown files. This helps maintain code quality and documentation standards.

## Configuration

The spellcheck configuration is defined in `_typos.toml`:

- **Files checked**: Rust source files (`*.rs`) and Markdown files (`*.md`)
- **Excluded**: Build artifacts, JavaScript files, and static assets
- **Custom dictionary**: Includes project-specific terms like `agave`, `solana`, `bpf`, etc.

## Usage

### Local Development

Run spellcheck on specific directories:
```bash
./scripts/spellcheck.sh [directory1] [directory2] ...
```

Run spellcheck on initial test crates (default):
```bash
./scripts/spellcheck.sh
```

### CI Integration

The spellcheck runs automatically in CI via `.github/workflows/spellcheck.yml`:
- Triggers on pushes and pull requests to master and version branches
- Currently limited to a few smaller crates for initial testing
- Can be expanded to cover the entire repository once validated

## Initial Rollout Strategy

1. **Phase 1**: Test on smaller crates (`clap-utils`, `clap-v3-utils`, `client-test`)
2. **Phase 2**: Expand to more crates based on results
3. **Phase 3**: Full repository coverage

## Adding New Terms

To add project-specific terms to the dictionary, edit `_typos.toml` and add entries to the `[default.extend-words]` section:

```toml
[default.extend-words]
newterm = "newterm"
```

## Fixing Spelling Errors

When the spellcheck finds errors:

1. Review the suggested corrections
2. Fix legitimate spelling mistakes
3. Add valid terms to the custom dictionary if they're false positives
4. Commit the fixes

## Benefits

- **Automated**: No manual review needed for basic spelling
- **Consistent**: Enforces spelling standards across the codebase
- **Efficient**: Catches errors early in the development process
- **Configurable**: Easy to customize for project-specific needs
