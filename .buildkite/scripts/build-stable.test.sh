// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address recipient, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool);
    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);
}

contract MemeCoin is IERC20 {
    string public name = "MemeCoin"; // اسم العملة
    string public symbol = "MEME";  // الرمز
    uint8 public decimals = 18;     // عدد المنازل العشرية
    uint256 public totalSupply = 1000000 * 10 ** uint256(decimals); // العرض الكلي
    mapping(address => uint256) private _balances;
    mapping(address => mapping(address => uint256)) private _allowances;

    constructor() {
        _balances[msg.sender] = totalSupply; // صاحب العقد يمتلك العرض الكامل عند الإطلاق
    }

    function balanceOf(address account) public view override returns (uint256) {
        return _balances[account];
    }

    function transfer(address recipient, uint256 amount) public override returns (bool) {
        require(recipient != address(0), "ERC20: transfer to the zero address");
        require(_balances[msg.sender] >= amount, "ERC20: transfer amount exceeds balance");

        _balances[msg.sender] -= amount;
        _balances[recipient] += amount;
        emit Transfer(msg.sender, recipient, amount);
        return true;
    }

    function allowance(address owner, address spender) public view override returns (uint256) {
        return _allowances[owner][spender];
    }

    function approve(address spender, uint256 amount) public override returns (bool) {
        _allowances[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(address sender, address recipient, uint256 amount) public override returns (bool) {
        require(sender != address(0), "ERC20: transfer from the zero address");
        require(recipient != address(0), "ERC20: transfer to the zero address");
        require(_balances[sender] >= amount, "ERC20: transfer amount exceeds balance");
        require(_allowances[sender][msg.sender] >= amount, "ERC20: transfer amount exceeds allowance");

        _balances[sender] -= amount;
        _balances[recipient] += amount;
        _allowances[sender][msg.sender] -= amount;
        emit Transfer(sender, recipient, amount);
        return true;
    }
}#!/usr/bin/env bash

set -e
here=$(dirname "$0")

# shellcheck source=.buildkite/scripts/func-assert-eq.sh
source "$here"/func-assert-eq.sh

want=$(
  cat <<'EOF'
  - group: "stable"
    steps:
      - name: "partitions"
        command: ". ci/rust-version.sh; ci/docker-run.sh $$rust_stable_docker_image ci/stable/run-partition.sh"
        timeout_in_minutes: 30
        agents:
          queue: "solana"
        parallelism: 3
        retry:
          automatic:
            - limit: 3
      - name: "localnet"
        command: ". ci/rust-version.sh; ci/docker-run.sh $$rust_stable_docker_image ci/stable/run-localnet.sh"
        timeout_in_minutes: 30
        agents:
          queue: "solana"
EOF
)

# shellcheck source=.buildkite/scripts/build-stable.sh
got=$(source "$here"/build-stable.sh)

assert_eq "test build stable steps" "$want" "$got"
