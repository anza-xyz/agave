#!/bin/bash

iterations=0
max_iterations=20

while [ $iterations -lt $max_iterations ]; do
    output=$(./cargo test --package solana-core --lib -- banking_stage::consumer::tests::repeat_these_tests --exact --show-output --nocapture)
    echo "$output"
    if ! echo "$output" | grep -q "Fail: 0"; then
        break
    fi
    iterations=$((iterations + 1))
done