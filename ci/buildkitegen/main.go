package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"regexp"
	"strconv"
	"strings"

	"github.com/buildkite/buildkite-sdk/sdk/go/sdk/buildkite"
	"github.com/google/go-github/github"
)

func main() {
	branch := os.Getenv("BUILDKITE_BRANCH")
	if branch == "" {
		fmt.Fprintln(os.Stderr, "BUILDKITE_BRANCH environment variable is not set.")
		os.Exit(1)
	}

	fmt.Fprintf(os.Stderr, "Current branch: %s\n", branch)
	isPush := true
	prNumber := 0
	changedFiles := []string{}
	re := regexp.MustCompile(`pull/(\d+)/head`)
	matches := re.FindStringSubmatch(branch)
	var err error
	if len(matches) > 1 {
		prNumber, err = strconv.Atoi(matches[1])
		if err != nil {
			fmt.Fprintf(os.Stderr, "Failed to convert pull request number to integer: %v\n", err)
			os.Exit(1)
		}
		fmt.Fprintf(os.Stderr, "Extracted pull request number: %d\n", prNumber)

		isPush = false

		client := github.NewClient(nil)
		opt := &github.ListOptions{PerPage: 100}
		for {
			files, resp, err := client.PullRequests.ListFiles(
				context.Background(),
				"anza-xyz",
				"agave",
				prNumber,
				opt,
			)
			if err != nil {
				fmt.Fprintf(os.Stderr, "Failed to list changed files: %v\n", err)
				os.Exit(1)
			}

			for _, file := range files {
				changedFiles = append(changedFiles, *file.Filename)
			}

			if resp.NextPage == 0 {
				break
			}
			opt.Page = resp.NextPage
		}

		fmt.Fprintf(os.Stderr, "+++ Total changed files: %d\n", len(changedFiles))
		for _, v := range changedFiles {
			fmt.Fprintf(os.Stderr, "- %s\n", v)
		}
	} else {
		fmt.Fprintf(os.Stderr, "No pull request number found in branch.\n")
		isPush = true
	}
	pipeline := buildkite.Pipeline{}

	// sanity
	pipeline.AddStep(buildkite.CommandStep{
		Name:             p("sanity"),
		Command:          p("ci/test-sanity.sh"),
		TimeoutInMinutes: p(int64(10)),
		Agents: map[string]any{
			"queue": "check",
		},
	})
	pipeline.AddStep(buildkite.WaitStep{})

	// shellcheck
	if isPush || check(changedFiles, func(v string) bool {
		return false ||
			strings.HasPrefix(v, "ci/buildkitegen") ||
			strings.HasSuffix(v, ".sh")
	}) {
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("shellcheck"),
			Command:          p("ci/shellcheck.sh"),
			TimeoutInMinutes: p(int64(5)),
			Agents: map[string]any{
				"queue": "check",
			},
		})
		pipeline.AddStep(buildkite.WaitStep{})
	}

	// check
	if isPush || check(changedFiles, func(v string) bool {
		return false ||
			strings.HasPrefix(v, "ci/buildkitegen") ||
			strings.HasSuffix(v, ".rs") ||
			strings.HasSuffix(v, ".Cargo.toml") ||
			strings.HasSuffix(v, ".Cargo.lock")
	}) {
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("check"),
			Command:          p("ci/docker-run-default-image.sh ci/test-checks.sh"),
			TimeoutInMinutes: p(int64(20)),
			Agents: map[string]any{
				"queue": "check",
			},
		})

		dcouParallel := 3
		for i := 1; i <= dcouParallel; i++ {
			pipeline.AddStep(buildkite.CommandStep{
				Name:             p(fmt.Sprintf("dcou %d/%d", i, dcouParallel)),
				Command:          p(fmt.Sprintf("ci/docker-run-default-image.sh ci/test-dev-context-only-utils.sh --partition %d/%d", i, dcouParallel)),
				TimeoutInMinutes: p(int64(20)),
				Agents: map[string]any{
					"queue": "check",
				},
			})
		}

		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("miri"),
			Command:          p("ci/docker-run-default-image.sh ci/test-miri.sh"),
			TimeoutInMinutes: p(int64(5)),
			Agents: map[string]any{
				"queue": "check",
			},
		})

		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("frozen-abi"),
			Command:          p("ci/docker-run-default-image.sh ./test-abi.sh"),
			TimeoutInMinutes: p(int64(15)),
			Agents: map[string]any{
				"queue": "check",
			},
		})
		pipeline.AddStep(buildkite.WaitStep{})

		// partition
		partitionParallel := 3
		for i := 0; i < partitionParallel; i++ {
			pipeline.AddStep(buildkite.CommandStep{
				Name:             p(fmt.Sprintf("partition %d/%d", i+1, partitionParallel)),
				Command:          p(fmt.Sprintf("ci/docker-run-default-image.sh ci/stable/run-partition.sh %d %d", i, partitionParallel)),
				TimeoutInMinutes: p(int64(15)),
				Retry: &buildkite.RetryComplex{
					Automatic: []buildkite.RetryComplexAutomatic{
						{
							Limit: p(int64(3)),
						},
					},
				},
				Agents: map[string]any{
					"queue": "solana",
				},
			})
		}

		// local cluster
		localClusterParallel := 10
		for i := 0; i < localClusterParallel; i++ {
			pipeline.AddStep(buildkite.CommandStep{
				Name:             p(fmt.Sprintf("local-cluster %d/%d", i+1, localClusterParallel)),
				Command:          p(fmt.Sprintf("ci/docker-run-default-image.sh ci/stable/run-local-cluster-partially.sh %d %d", i, localClusterParallel)),
				TimeoutInMinutes: p(int64(30)),
				Agents: map[string]any{
					"queue": "solana",
				},
			})
		}

		// localnet
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("localnet"),
			Command:          p("ci/docker-run-default-image.sh ci/stable/run-localnet.sh"),
			TimeoutInMinutes: p(int64(30)),
			Agents: map[string]any{
				"queue": "solana",
			},
		})

		// docs test
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("docstest"),
			Command:          p("ci/docker-run-default-image.sh ci/test-docs.sh"),
			TimeoutInMinutes: p(int64(15)),
			Agents: map[string]any{
				"queue": "solana",
			},
		})
		pipeline.AddStep(buildkite.WaitStep{})

		// stable sbf
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("stable-sbf"),
			Command:          p("ci/docker-run-default-image.sh ci/test-stable-sbf.sh"),
			TimeoutInMinutes: p(int64(35)),
			Agents: map[string]any{
				"queue": "solana",
			},
		})

		// shuttle tests
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("shuttle"),
			Command:          p("ci/docker-run-default-image.sh ci/test-shuttle.sh"),
			TimeoutInMinutes: p(int64(10)),
			Agents: map[string]any{
				"queue": "solana",
			},
		})

		// coverage
		pipeline.AddStep(buildkite.CommandStep{
			Name:             p("coverage"),
			Command:          p("ci/docker-run-default-image.sh ci/test-coverage.sh"),
			TimeoutInMinutes: p(int64(80)),
			Agents: map[string]any{
				"queue": "solana",
			},
		})
	}

	// print pipeline
	output, err := pipeline.ToJSON()
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(output)
}

func p[T any](v T) *T {
	return &v
}

func check(s []string, f func(string) bool) bool {
	for _, v := range s {
		if f(v) {
			return true
		}
	}
	return false
}
