# Runbook \u2014 First-boot diagnostics

After flashing install media and powering the box on, the COM1 serial
console is the single source of truth. This runbook walks through
the expected log sequence and what each anomaly means.

## Expected log (success path)

```
celhyper: alive
celhyper: vm namespace constructed
celhyper: vmx runtime initialised
celhyper: installing host gdt+tss...
celhyper: host gdt+tss installed
lapic_base=0xfee00000
lapic_id=0x0
lapic_version=0x...
smp_cpu_count=0x1
smp_bsp_apic_id=0x0
vm_a_id=0x1
vm_b_id=0x2
vm_count=0x2
celhyper: vm namespace path round-trip ok
rr_launched=0x2
ls_vm_id=0x1
ls_vm_state_raw=0x2          # 2 = Halted
ls_vm_exit=0xc               # 12 = HLT
GUEST OK \u2014 Celium Guest Alive!
metrics_vm_exits_total=0x2
metrics_vm_exits_hlt=0x2
celhyper: bring_up complete
```

## Anomalies and remediation

| Anomaly | Cause | Action |
|---|---|---|
| `no VMX and no SVM` | CPU lacks hardware virtualization. | Enable VT-x in firmware; check `cpuid.1.ecx[5]`. |
| `celhyper: smp: handoff topology invalid` | CelLoader passed `cpu_count=0` or inconsistent AP array. | Re-build celloader with `--features real-handoff`. |
| `celhyper: lapic init failed` | `IA32_APIC_BASE` MMIO at non-default address and firmware never enabled the LAPIC. | Check UEFI firmware setup; LAPIC must be enabled. Boot continues single-CPU without IPIs. |
| `celhyper: vm-exit reason unreadable` | `EXIT_REASON` VMCS field could not be read; CPU rejected `vmlaunch`. | Almost always nested-VMX limitation; try real hardware. |
| `vm_state_raw=0x4` (Faulted) | Guest crashed at boot. | Inspect `guest_rip` and `exit_qualification` log lines just above. |

## Capturing the log

* QEMU: `-serial stdio` or `-serial tcp:host:port`.
* Physical: USB-RS232 dongle at 38400 8N1, no flow control.

If the log stops before `celhyper: bring_up complete`, capture
everything from `celhyper: alive` onwards and file a bug with the
exact line that came last \u2014 each line tells you which kernel phase
was running.

## Health check after a successful boot

The kernel now sits in `bridge::run()` waiting for NDJSON requests
on COM2. From the host:

```bash
nc <kernel-com2-host> <port>
# Or, programmatically, via celctl:
export CELIUM_BRIDGE_TCP=host:port
celctl cluster vms --vm-host celhyper-serial:$CELIUM_BRIDGE_TCP
```

A successful `vms list` returns the two boot VMs in `Halted` state.
