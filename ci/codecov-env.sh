#!/usr/bin/env bash

# Copied from https://github.com/codecov/codecov-bash/blob/43c5d3c7147b91674dd69c3e0e0becc48eba04bd/env

# Apache License Version 2.0, January 2004
# https://github.com/codecov/codecov-bash/blob/master/LICENSE

set -e +o pipefail

VERSION="1.0.6"

add()
{
  if [ -z "$1" ];
  then
    return
  fi

  echo -n "-e $1 "
}

add "CODECOV_ENV"
add "CODECOV_TOKEN"
add "CODECOV_URL"
add "CODECOV_SLUG"
add "VCS_COMMIT_ID"
add "VCS_BRANCH_NAME"
add "VCS_PULL_REQUEST"
add "VCS_SLUG"
add "VCS_TAG"
add "CI_BUILD_URL"
add "CI_BUILD_ID"
add "CI_JOB_ID"

if [ "$JENKINS_URL" != "" ];
then
  add "JENKINS_URL"
  add "ghprbSourceBranch"
  add "GIT_BRANCH"
  add "ghprbActualCommit"
  add "GIT_COMMIT"
  add "CHANGE_ID"
  add "BRANCH_NAME"
  add "BUILD_NUMBER"
  add "ghprbPullId"
  add "BUILD_URL"

elif [ "$CI" = "true" ] && [ "$TRAVIS" = "true" ] && [ "$SHIPPABLE" != "true" ];
then
  add "CI"
  add "TRAVIS"
  add "SHIPPABLE"
  add "TRAVIS_BRANCH"
  add "TRAVIS_COMMIT"
  add "TRAVIS_JOB_NUMBER"
  add "TRAVIS_PULL_REQUEST"
  add "TRAVIS_JOB_ID"
  add "TRAVIS_REPO_SLUG"
  add "TRAVIS_TAG"
  add "TRAVIS_OS_NAME"

elif [ "$CODEBUILD_CI" = "true" ];
then
  add "CODEBUILD_WEBHOOK_HEAD_REF"
  add "CODEBUILD_RESOLVED_SOURCE_VERSION"
  add "CODEBUILD_SOURCE_VERSION"
  add "CODEBUILD_BUILD_ID"
  add "CODEBUILD_SOURCE_REPO_URL"

elif [ "$DOCKER_REPO" != "" ];
then
  add "SOURCE_BRANCH"
  add "SOURCE_COMMIT"
  add "DOCER_REPO"
  add "CACHE_TAG"

elif [ "$CI" = "true" ] && [ "$CI_NAME" = "codeship" ];
then
  add "CI"
  add "CI_NAME"
  add "CI_BRANCH"
  add "CI_BUILD_NUMBER"
  add "CI_BUILD_URL"
  add "CI_COMMIT_ID"

elif [ -n "$CF_BUILD_URL" ] && [ -n "$CF_BUILD_ID" ];
then
  add "CF_BRANCH"
  add "CF_BUILD_ID"
  add "CF_BUILD_URL"
  add "CF_REVISION"

elif [ "$TEAMCITY_VERSION" != "" ];
then
  add "TEAMCITY_VERSION"
  add "TEAMCITY_BUILD_BRANCH"
  add "TEAMCITY_BUILD_ID"
  add "TEAMCITY_BUILD_URL"
  add "TEAMCITY_BUILD_COMMIT"
  add "TEAMCITY_BUILD_COMMIT"
  add "BUILD_VCS_NUMBER"
  add "TEAMCITY_BUILD_REPOSITORY"

elif [ "$CI" = "true" ] && [ "$CIRCLECI" = "true" ];
then
  add "CI"
  add "CIRCLECI"
  add "CIRCLE_BRANCH"
  add "CIRCLE_BUILD_NUM"
  add "CIRCLE_NODE_INDEX"
  add "CIRCLE_PROJECT_USERNAME"
  add "CIRCLE_PROJECT_REPONAME"
  add "CIRCLE_REPOSITORY_URL"
  add "CIRCLE_PR_NUMBER"
  add "CIRCLE_SHA1"
  add "CIRCLE_ARTIFACTS"
  add "CIRCLE_TEST_REPORTS"

elif [ "$BUDDYBUILD_BRANCH" != "" ];
then
  add "BUDDYBUILD_BRANCH"
  add "BUDDYBUILD_BUILD_NUMBER"
  add "BUDDYBUILD_BUILD_ID"
  add "BUDDYBUILD_APP_ID"

elif [ "${bamboo_planRepository_revision}" != "" ];
then
  add "bamboo_planRepository_revision"
  add "bamboo_planRepository_branch"
  add "bamboo_buildNumber"
  add "bamboo_buildResultsUrl"
  add "bamboo_planRepository_repositoryUrl"

elif [ "$CI" = "true" ] && [ "$BITRISE_IO" = "true" ];
then
  add "CI"
  add "BITRISE_IO"
  add "BITRISE_GIT_BRANCH"
  add "BITRISE_BUILD_NUMBER"
  add "BITRISE_BUILD_URL"
  add "BITRISE_PULL_REQUEST"
  add "BITRISE_GIT_COMMIT"

# https://docs.semaphoreci.com/ci-cd-environment/environment-variables/#semaphore-related
elif [ "$CI" = "true" ] && [ "$SEMAPHORE" = "true" ];
then
  add "CI"
  add "SEMAPHORE"
  add "SEMAPHORE_PROJECT_NAME"
  add "SEMAPHORE_PROJECT_ID"
  add "SEMAPHORE_ORGANIZATION_URL"
  add "SEMAPHORE_JOB_NAME"
  add "SEMAPHORE_WORKFLOW_NUMBER"
  add "SEMAPHORE_JOB_ID"
  add "SEMAPHORE_GIT_SHA"
  add "SEMAPHORE_GIT_BRANCH"
  add "SEMAPHORE_GIT_REPO_SLUG"
  add "SEMAPHORE_GIT_PR_NUMBER"

elif [ "$CI" = "true" ] && [ "$BUILDKITE" = "true" ];
then
  add "CI"
  add "BUILDKITE"
  add "BUILDKITE_BRANCH"
  add "BUILDKITE_BUILD_NUMBER"
  add "BUILDKITE_JOB_ID"
  add "BUILDKITE_BUILD_URL"
  add "BUILDKITE_PROJECT_SLUG"
  add "BUILDKITE_COMMIT"

elif [ "$CI" = "drone" ] || [ "$DRONE" = "true" ];
then
  add "CI"
  add "DRONE_BRANCH"
  add "DRONE_BUILD_NUMBER"
  add "DRONE_BUILD_LINK"
  add "DRONE_PULL_REQUEST"
  add "DRONE_JOB_NUMBER"
  add "DRONE_TAG"
  add "CI_BUILD_URL"

elif [ "$CI" = "true" ] && [ "$HEROKU_TEST_RUN_BRANCH" != "" ];
then
  add "HEROKU_TEST_RUN_BRANCH"
  add "HEROKU_TEST_RUN_ID"
  add "HEROKU_TEST_RUN_COMMIT_VERSION"

elif [[ "$CI" = "true" || "$CI" = "True" ]] && [[ "$APPVEYOR" = "true" || "$APPVEYOR" = "True" ]];
then
  add "CI"
  add "APPVEYOR"
  add "APPVEYOR_REPO_BRANCH"
  add "APPVEYOR_JOB_ID"
  add "APPVEYOR_PULL_REQUEST_NUMBER"
  add "APPVEYOR_ACCOUNT_NAME"
  add "APPVEYOR_PROJECT_SLUG"
  add "APPVEYOR_BUILD_VERSION"
  add "APPVEYOR_REPO_NAME"
  add "APPVEYOR_REPO_COMMIT"

elif [ "$CI" = "true" ] && [ "$WERCKER_GIT_BRANCH" != "" ];
then
  add "CI"
  add "WERCKER_GIT_BRANCH"
  add "WERCKER_MAIN_PIPELINE_STARTED"
  add "WERCKER_GIT_OWNER"
  add "WERCKER_GIT_REPOSITORY"
  add "WERCKER_GIT_COMMIT"

elif [ "$CI" = "true" ] && [ "$MAGNUM" = "true" ];
then
  add "CI"
  add "MAGNUM"
  add "CI_BRANCH"
  add "CI_BUILD_NUMBER"
  add "CI_COMMIT"

elif [ "$SHIPPABLE" = "true" ];
then
  add "SHIPPABLE"
  add "HEAD_BRANCH"
  add "BRANCH"
  add "BUILD_NUMBER"
  add "BUILD_URL"
  add "PULL_REQUEST"
  add "REPO_FULL_NAME"
  add "COMMIT"

elif [ "$TDDIUM" = "true" ];
then
  add "TDDIUM"
  add "TDDIUM_CURRENT_COMMIT"
  add "TDDIUM_CURRENT_BRANCH"
  add "TDDIUM_TID"
  add "TDDIUM_PR_ID"

elif [ "$GREENHOUSE" = "true" ];
then
  add "GREENHOUSE"
  add "GREENHOUSE_BRANCH"
  add "GREENHOUSE_BUILD_NUMBER"
  add "GREENHOUSE_BUILD_URL"
  add "GREENHOUSE_PULL_REQUES"
  add "GREENHOUSE_COMMIT"
  add "GREENHOUSE_EXPORT_DIR"

elif [ "$GITLAB_CI" != "" ];
then
  add "GITLAB_CI"
  add "CI_BUILD_REF_NAME"
  add "CI_BUILD_ID"
  add "CI_BUILD_REPO"
  add "CI_BUILD_REF"

elif [ "$GITHUB_ACTIONS" != '' ]
then
  add "GITHUB_ACTIONS"
  add "GITHUB_HEAD_REF"
  add "GITHUB_REF"
  add "GITHUB_REPOSITORY"
  add "GITHUB_RUN_ID"
  add "GITHUB_SERVER_URL"
  add "GITHUB_SHA"
  add "GITHUB_WORKFLOW"

elif [ "$SYSTEM_TEAMFOUNDATIONSERVERURI" != "" ];
then
  add "BUILD_SOURCEVERSION"
  add "BUILD_BUILDNUMBER"
  add "SYSTEM_PULLREQUEST_PULLREQUESTNUMBER"
  add "SYSTEM_PULLREQUEST_PULLREQUESTID"
  add "SYSTEM_TEAMPROJECT"
  add "SYSTEM_TEAMFOUNDATIONSERVERURI"
  add "BUILD_BUILDID"

elif [ "$CI" = "true" ] && [ "$BITBUCKET_BUILD_NUMBER" != "" ];
then
  add "CI"
  add "BITBUCKET_BRANCH"
  add "BITBUCKET_BUILD_NUMBER"
  add "BITBUCKET_REPO_OWNER"
  add "BITBUCKET_REPO_SLUG"
  add "BITBUCKET_PR_ID"
  add "BITBUCKET_COMMIT"

elif [ "$CI" = "true" ] && [ "$BUDDY" = "true" ];
then
  add "CI"
  add "BUDDY"
  add "BUDDY_EXECUTION_BRANCH"
  add "BUDDY_EXECUTION_REVISION"
  add "BUDDY_EXECUTION_ID"
  add "BUDDY_EXECUTION_URL"
  add "BUDDY_EXECUTION_PULL_REQUEST_NO"
  add "BUDDY_EXECUTION_TAG"
  add "BUDDY_REPO_SLUG"

elif [ "$CIRRUS_CI" != "" ];
then
  add "CIRRUS_REPO_FULL_NAME"
  add "CIRRUS_BRANCH"
  add "CIRRUS_PR"
  add "CIRRUS_CHANGE_IN_REPO"
  add "CIRRUS_TASK_ID"
  add "CIRRUS_TASK_NAME"

fi
