#!/usr/bin/env bash
set -euo pipefail

team_to_add_slug="security-incident-response"
github_org="anza-xyz"
github_repo="agave"

echo "Adding $team_to_add_slug"

# Note: This will get all the GHSAs even if there are more than the per_page value
# from gh api --help
# --paginate    Make additional HTTP requests to fetch all pages of results
ghsa_json=$(gh api \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28"   \
    /repos/$github_org/$github_repo/security-advisories?per_page=100 --paginate )

# Get a list of GHSAs that don't have the $team_to_add_slug in collaborating_teams
ghsa_without_team=$( jq -r '[ .[] | select(all(.collaborating_teams.[]; .slug != "'$team_to_add_slug'")) | .ghsa_id ] | sort | .[] ' <<< "$ghsa_json" )
echo "ghsa_without_team: $ghsa_without_team"

# Iterate through the teams
while IFS= read -r ghsa_id; do
    echo "ghsa_id: $ghsa_id"
    # PATCH updates the value. If we just set -f "collaborating_teams[]=$team_to_add_slug" it
    # will overwrite any existing collaborating_teams. So we get the list of teams that are already
    # added to this GHSA and format them as parameters for gh api like:
    #    -f collaborating_teams[]=ghsa-testing-1
    original_collaborating_team_slugs=$( jq -r '[ .[] | select(.ghsa_id == "'$ghsa_id'") | .collaborating_teams ] | "-f collaborating_teams[]=" + .[][].slug ' <<< "$ghsa_json" )
    echo "original_collaborating_team_slugs: $original_collaborating_team_slugs"
    
    # Update the team list
    gh api \
        --method PATCH \
        -H "Accept: application/vnd.github+json" \
        -H "X-GitHub-Api-Version: 2022-11-28" \
        "/repos/$github_org/$github_repo/security-advisories/$ghsa_id" \
        -f "collaborating_teams[]=$team_to_add_slug" $original_collaborating_team_slugs
done <<< "$ghsa_without_team"
