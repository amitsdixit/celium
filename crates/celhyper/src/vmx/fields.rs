//! VMCS field encodings. Subset of Intel SDM Vol 3 Appendix B used by the
//! Week-2 first-guest launch path. Add encodings here as new fields are
//! consumed; keep them grouped by category so the table stays auditable.
//!
//! The encoding itself is a 32-bit value; the field width (16/32/64/natural)
//! is implied by bits 14:13 of the encoding and is what tells `vmread`
//! whether to expect 32 or 64 bits back.

#![allow(missing_docs)] // self-documenting tables

// ---------- 16-bit guest-state fields --------------------------------------
pub const GUEST_ES_SELECTOR:        u32 = 0x0000_0800;
pub const GUEST_CS_SELECTOR:        u32 = 0x0000_0802;
pub const GUEST_SS_SELECTOR:        u32 = 0x0000_0804;
pub const GUEST_DS_SELECTOR:        u32 = 0x0000_0806;
pub const GUEST_FS_SELECTOR:        u32 = 0x0000_0808;
pub const GUEST_GS_SELECTOR:        u32 = 0x0000_080A;
pub const GUEST_LDTR_SELECTOR:      u32 = 0x0000_080C;
pub const GUEST_TR_SELECTOR:        u32 = 0x0000_080E;

// ---------- 16-bit host-state fields ---------------------------------------
pub const HOST_ES_SELECTOR:         u32 = 0x0000_0C00;
pub const HOST_CS_SELECTOR:         u32 = 0x0000_0C02;
pub const HOST_SS_SELECTOR:         u32 = 0x0000_0C04;
pub const HOST_DS_SELECTOR:         u32 = 0x0000_0C06;
pub const HOST_FS_SELECTOR:         u32 = 0x0000_0C08;
pub const HOST_GS_SELECTOR:         u32 = 0x0000_0C0A;
pub const HOST_TR_SELECTOR:         u32 = 0x0000_0C0C;

// ---------- 64-bit control fields ------------------------------------------
pub const IO_BITMAP_A:              u32 = 0x0000_2000;
pub const IO_BITMAP_B:              u32 = 0x0000_2002;
pub const MSR_BITMAP:               u32 = 0x0000_2004;
pub const VM_EXIT_MSR_STORE_ADDR:   u32 = 0x0000_2006;
pub const VM_EXIT_MSR_LOAD_ADDR:    u32 = 0x0000_2008;
pub const VM_ENTRY_MSR_LOAD_ADDR:   u32 = 0x0000_200A;
pub const EPT_POINTER:              u32 = 0x0000_201A;

// ---------- 32-bit control fields ------------------------------------------
pub const PIN_BASED_VM_EXEC_CTL:    u32 = 0x0000_4000;
pub const CPU_BASED_VM_EXEC_CTL:    u32 = 0x0000_4002;
pub const EXCEPTION_BITMAP:         u32 = 0x0000_4004;
pub const VM_EXIT_CTLS:             u32 = 0x0000_400C;
pub const VM_ENTRY_CTLS:            u32 = 0x0000_4012;
pub const SECONDARY_VM_EXEC_CTL:    u32 = 0x0000_401E;

// ---------- 32-bit RO data fields ------------------------------------------
pub const VM_INSTRUCTION_ERROR:     u32 = 0x0000_4400;
pub const EXIT_REASON:              u32 = 0x0000_4402;

// ---------- Natural-width guest-state fields -------------------------------
pub const GUEST_CR0:                u32 = 0x0000_6800;
pub const GUEST_CR3:                u32 = 0x0000_6802;
pub const GUEST_CR4:                u32 = 0x0000_6804;
pub const GUEST_RSP:                u32 = 0x0000_681C;
pub const GUEST_RIP:                u32 = 0x0000_681E;
pub const GUEST_RFLAGS:             u32 = 0x0000_6820;

// ---------- Natural-width host-state fields --------------------------------
pub const HOST_CR0:                 u32 = 0x0000_6C00;
pub const HOST_CR3:                 u32 = 0x0000_6C02;
pub const HOST_CR4:                 u32 = 0x0000_6C04;
pub const HOST_FS_BASE:             u32 = 0x0000_6C06;
pub const HOST_GS_BASE:             u32 = 0x0000_6C08;
pub const HOST_TR_BASE:             u32 = 0x0000_6C0A;
pub const HOST_GDTR_BASE:           u32 = 0x0000_6C0C;
pub const HOST_IDTR_BASE:           u32 = 0x0000_6C0E;
pub const HOST_RSP:                 u32 = 0x0000_6C14;
pub const HOST_RIP:                 u32 = 0x0000_6C16;

// ---------- Exit-information natural-width fields --------------------------
pub const EXIT_QUALIFICATION:       u32 = 0x0000_6400;
pub const GUEST_LINEAR_ADDRESS:     u32 = 0x0000_640A;

// ---------- Exit reasons (subset, SDM Vol 3 App C) -------------------------
/// Guest executed `HLT` while CPU-based control bit 7 was 1.
pub const EXIT_REASON_HLT: u32 = 12;
/// Guest executed `MOV CR`.
pub const EXIT_REASON_CR_ACCESS: u32 = 28;
/// EPT violation (guest accessed page with insufficient permissions).
pub const EXIT_REASON_EPT_VIOLATION: u32 = 48;

// ---------- Pin-based control bits (subset) --------------------------------
pub const PINBASED_EXT_INTR_EXITING:    u32 = 1 << 0;
pub const PINBASED_NMI_EXITING:         u32 = 1 << 3;

// ---------- Primary processor-based control bits (subset) ------------------
pub const CPUBASED_HLT_EXITING:           u32 = 1 << 7;
pub const CPUBASED_USE_MSR_BITMAPS:       u32 = 1 << 28;
pub const CPUBASED_ACTIVATE_SECONDARY:    u32 = 1 << 31;

// ---------- Secondary processor-based control bits (subset) ----------------
pub const SECONDARY_ENABLE_EPT:           u32 = 1 << 1;
pub const SECONDARY_UNRESTRICTED_GUEST:   u32 = 1 << 7;

// ---------- VM-exit / entry control bits (subset) --------------------------
pub const VMEXIT_HOST_ADDR_SPACE_SIZE:    u32 = 1 << 9;  // host returns to long mode
pub const VMENTRY_IA32E_MODE_GUEST:       u32 = 1 << 9;  // guest runs in long mode
