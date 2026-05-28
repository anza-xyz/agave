---
title: Running with AF_XDP
sidebar_label: Running with AF_XDP
sidebar_position: 7
---

XDP: eXpress Data Path is a Linux kernel technology that allows developers to write high-performance networking code which bypasses the kernel’s usual packet handling path. This means fewer data copies and fewer context switches between user and kernel space. By handling packets directly with the network interface card in user space, XDP greatly reduces the overhead per packet.

Agave version 3.0.9 and later supports XDP for Turbine packet handling. XDP works most efficiently with a NIC that has XDP driver support. Early testing indicates that 100M-CU blocks are achievable if operators adopt XDP in validator operations, providing the headroom needed to scale block propagation.

## Configuration

Before rolling out XDP on a production validator, you should test it on your setup and verify a few things:
* **Driver Compatibility:** No unexpected NIC driver or hardware issues when XDP is enabled on your system.
* **Performance Gain:** Confirm that performance is improved with the new configuration (e.g. lower CPU usage or higher throughput in Turbine’s retransmit stage).
* **Metric Visibility:** Verify that you can observe the retransmit-stage metrics, which show time spent sending shreds, to gauge the impact of XDP on network transmission.

To enable XDP in Agave, add the following command-line flags to your validator startup command (using Agave v3.0.9+):

```bash
--experimental-retransmit-xdp-cpu-cores 1
--experimental-retransmit-xdp-zero-copy # Do NOT pass this flag when using the bnxt_en driver.
--experimental-poh-pinned-cpu-core 10
```

Note that --experimental-retransmit-xdp-zero-copy will avoid using socket buffers for data, but this is only possible when talking directly to the Network Interface Card (NIC). As a result, zero copy cannot be used with the bonded interface itself. When using a bonded network interface, specify the underlying member interface to which the XDP program should be attached:

```bash
--experimental-retransmit-xdp-interface <bond-member-interface>
```

 Also note that XDP and PoH *must* be assigned to separate (physical) cores. The
--experimental-poh-pinned-cpu-core N flag can be used to move the PoH thread.

Next, your validator binary will need to have access to a few higher level permissions. The validator process requires the CAP_NET_RAW, CAP_NET_ADMIN, CAP_BPF, and CAP_PERFMON capabilities. These can be configured in the systemd service file by setting CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN CAP_BPF CAP_PERFMON under the [Service] section or directly on the binary with the command:

```bash
sudo setcap cap_net_raw,cap_net_admin,cap_bpf,cap_perfmon=p <path/to/agave-validator>
#this command must be run each time the binary is replaced
```

The setcap stores the updated privileges on the binary file, so this command will need to be rerun any time the binary is upgraded.

## Conclusion

Enabling XDP in Agave allows a validator to send data out more efficiently (fewer copies and syscalls), which translates to faster block propagation and more headroom for future growth. For now, we encourage a subset validator operators, especially nodes with XDP-capable NICs, to try it out and report any issues to #validator-support in the Solana Tech Discord. Thank you for contributing to Solana and helping the cluster prepare for 100M CUs.

## Troubleshooting

Some driver versions seem to return non-power of 2 queue sizes, which can cause issues. Setting these explicitly resolves the problem. For example, if the queues return a size of 511, forcing to 512 resolves things.

```bash
sudo ethtool -G enp196s0f0np0 rx 512 tx 512
```

The igb driver supports zero copy starting from kernel version 6.14. For all other drivers, kernel version 6.8 or newer is recommended.

## Debug Data Collection

When encountering issues, understanding the kernel, NIC, and driver information will be crucial to being able to debug issues

```bash
uname -a
sudo lshw -c network | grep 'logical name'
sudo ethtool -i <nic logical name>
sudo ethtool -g <nic logical name>
lspci | grep Ethernet
modinfo bnxt_en
```


## XDP / Zero-Copy Driver Support Matrix

| Driver / NIC family | AF_XDP w/o ZC | AF_XDP w/ ZC | Status |
| --- | --- | --- | --- |
| `mlx5` / Mellanox ConnectX | ✅ Works | ✅ Works | Operator reports: `mlx5` works with XDP + ZC on kernel 6.8; ConnectX-6 Lx worked after kernel 6.17 upgrade. Highest-confidence family in the discussion. |
| `i40e` / Intel 700 series | ✅ Works | ✅ Works | Operator report: `i40e` works with XDP + ZC on kernel 6.8. |
| `igb` / Intel I210 | ✅ Works | ✅ Works w/ caveat | caveat: `igb` requires kernel `>= 6.14` for ZC. Field report: I210 on 6.17 enabled ZC but had severe network degradation/high skips, so fall back to non-ZC if unstable. |
| `ixgbe` / Intel X540, X550 | ✅ Works | ⚠️ Mixed / unstable | Alessandro guidance for freeze/link-flap cases: start without ZC while `ixgbe` is debugged. Stay tuned! |
| `ice` / Intel E800 | ✅ Works | ✅ Works | `ice` supports native XDP and AF_XDP zero-copy. Caveats: XDP is blocked for frame sizes larger than 3KB |
| `bnxt_en` / Broadcom | ✅ Works | ❌ Does not work | `bnxt_en` works with XDP, but do not pass the zero-copy flag. Broadcom non-ZC can still be reasonably fast. But please get a non-broadcom NIC |
| `tg3` / Broadcom | ❌ No native/driver XDP; generic XDP only at best | ❌ Does not work | Broadcom BCM5720 uses the `tg3` driver. Treat as unsupported for Agave/AF_XDP performance work: no native XDP and no AF_XDP zero-copy. |
| `r8169` / Realtek | ❌ No native/driver XDP; generic XDP only at best | ❌ Does not work | Realtek NICs using `r8169` should be treated as unsupported for Agave/AF_XDP performance work: no native XDP and no AF_XDP zero-copy.|
| `mlx4_en` / Mellanox ConnectX-3 | ❌ Do not use | ❌ Does not work | Driver is no longer supported. Zero-copy does not work. Do not use. |
