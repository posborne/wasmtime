use crate::ir::KnownSymbol;
use crate::ir::immediates::{Ieee32, Ieee64};
use crate::isa::x64::encoding::evex::{EvexInstruction, EvexVectorLength, RegisterOrAmode};
use crate::isa::x64::encoding::rex::{
    LegacyPrefixes, OpcodeMap, RexFlags, emit_simm, emit_std_enc_enc, emit_std_enc_mem,
    emit_std_reg_mem, emit_std_reg_reg, int_reg_enc, low8_will_sign_extend_to_32, reg_enc,
};
use crate::isa::x64::encoding::vex::{VexInstruction, VexVectorLength};
use crate::isa::x64::external::PairedGpr;
use crate::isa::x64::inst::args::*;
use crate::isa::x64::inst::*;
use crate::isa::x64::lower::isle::generated_code::{Atomic128RmwSeqOp, AtomicRmwSeqOp};
use cranelift_assembler_x64 as asm;

/// A small helper to generate a signed conversion instruction.
fn emit_signed_cvt(
    sink: &mut MachBuffer<Inst>,
    info: &EmitInfo,
    state: &mut EmitState,
    src: Reg,
    dst: Writable<Reg>,
    to_f64: bool,
) {
    assert!(src.is_real());
    assert!(dst.to_reg().is_real());

    // Handle an unsigned int, which is the "easy" case: a signed conversion
    // will do the right thing.
    let dst = WritableXmm::from_writable_reg(dst).unwrap();
    let inst = if to_f64 {
        asm::inst::cvtsi2sdq_a::new(dst, src).into()
    } else {
        asm::inst::cvtsi2ssq_a::new(dst, src).into()
    };
    Inst::External { inst }.emit(sink, info, state);
}

/// Emits a one way conditional jump if CC is set (true).
fn one_way_jmp(sink: &mut MachBuffer<Inst>, cc: CC, label: MachLabel) {
    let cond_start = sink.cur_offset();
    let cond_disp_off = cond_start + 2;
    sink.use_label_at_offset(cond_disp_off, label, LabelUse::JmpRel32);
    sink.put1(0x0F);
    sink.put1(0x80 + cc.get_enc());
    sink.put4(0x0);
}

/// Emits a relocation, attaching the current source location as well.
fn emit_reloc(sink: &mut MachBuffer<Inst>, kind: Reloc, name: &ExternalName, addend: Addend) {
    sink.add_reloc(kind, name, addend);
}

/// The top-level emit function.
///
/// Important!  Do not add improved (shortened) encoding cases to existing
/// instructions without also adding tests for those improved encodings.  That
/// is a dangerous game that leads to hard-to-track-down errors in the emitted
/// code.
///
/// For all instructions, make sure to have test coverage for all of the
/// following situations.  Do this by creating the cross product resulting from
/// applying the following rules to each operand:
///
/// (1) for any insn that mentions a register: one test using a register from
///     the group [rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi] and a second one
///     using a register from the group [r8, r9, r10, r11, r12, r13, r14, r15].
///     This helps detect incorrect REX prefix construction.
///
/// (2) for any insn that mentions a byte register: one test for each of the
///     four encoding groups [al, cl, dl, bl], [spl, bpl, sil, dil],
///     [r8b .. r11b] and [r12b .. r15b].  This checks that
///     apparently-redundant REX prefixes are retained when required.
///
/// (3) for any insn that contains an immediate field, check the following
///     cases: field is zero, field is in simm8 range (-128 .. 127), field is
///     in simm32 range (-0x8000_0000 .. 0x7FFF_FFFF).  This is because some
///     instructions that require a 32-bit immediate have a short-form encoding
///     when the imm is in simm8 range.
///
/// Rules (1), (2) and (3) don't apply for registers within address expressions
/// (`Addr`s).  Those are already pretty well tested, and the registers in them
/// don't have any effect on the containing instruction (apart from possibly
/// require REX prefix bits).
///
/// When choosing registers for a test, avoid using registers with the same
/// offset within a given group.  For example, don't use rax and r8, since they
/// both have the lowest 3 bits as 000, and so the test won't detect errors
/// where those 3-bit register sub-fields are confused by the emitter.  Instead
/// use (eg) rax (lo3 = 000) and r9 (lo3 = 001).  Similarly, don't use (eg) cl
/// and bpl since they have the same offset in their group; use instead (eg) cl
/// and sil.
///
/// For all instructions, also add a test that uses only low-half registers
/// (rax .. rdi, xmm0 .. xmm7) etc, so as to check that any redundant REX
/// prefixes are correctly omitted.  This low-half restriction must apply to
/// _all_ registers in the insn, even those in address expressions.
///
/// Following these rules creates large numbers of test cases, but it's the
/// only way to make the emitter reliable.
///
/// Known possible improvements:
///
/// * there's a shorter encoding for shl/shr/sar by a 1-bit immediate.  (Do we
///   care?)
pub(crate) fn emit(
    inst: &Inst,
    sink: &mut MachBuffer<Inst>,
    info: &EmitInfo,
    state: &mut EmitState,
) {
    let matches_isa_flags = |iset_requirement: &InstructionSet| -> bool {
        match iset_requirement {
            // Cranelift assumes SSE2 at least.
            InstructionSet::SSE | InstructionSet::SSE2 => true,
            InstructionSet::CMPXCHG16b => info.isa_flags.use_cmpxchg16b(),
            InstructionSet::SSE3 => info.isa_flags.use_sse3(),
            InstructionSet::SSSE3 => info.isa_flags.use_ssse3(),
            InstructionSet::SSE41 => info.isa_flags.use_sse41(),
            InstructionSet::SSE42 => info.isa_flags.use_sse42(),
            InstructionSet::Popcnt => info.isa_flags.use_popcnt(),
            InstructionSet::Lzcnt => info.isa_flags.use_lzcnt(),
            InstructionSet::BMI1 => info.isa_flags.use_bmi1(),
            InstructionSet::BMI2 => info.isa_flags.has_bmi2(),
            InstructionSet::FMA => info.isa_flags.has_fma(),
            InstructionSet::AVX => info.isa_flags.has_avx(),
            InstructionSet::AVX2 => info.isa_flags.has_avx2(),
            InstructionSet::AVX512BITALG => info.isa_flags.has_avx512bitalg(),
            InstructionSet::AVX512DQ => info.isa_flags.has_avx512dq(),
            InstructionSet::AVX512F => info.isa_flags.has_avx512f(),
            InstructionSet::AVX512VBMI => info.isa_flags.has_avx512vbmi(),
            InstructionSet::AVX512VL => info.isa_flags.has_avx512vl(),
        }
    };

    // Certain instructions may be present in more than one ISA feature set; we must at least match
    // one of them in the target CPU.
    let isa_requirements = inst.available_in_any_isa();
    if !isa_requirements.is_empty() && !isa_requirements.iter().all(matches_isa_flags) {
        panic!(
            "Cannot emit inst '{inst:?}' for target; failed to match ISA requirements: {isa_requirements:?}"
        )
    }
    match inst {
        Inst::CheckedSRemSeq { divisor, .. } | Inst::CheckedSRemSeq8 { divisor, .. } => {
            // Validate that the register constraints of the dividend and the
            // destination are all as expected.
            let (dst, size) = match inst {
                Inst::CheckedSRemSeq {
                    dividend_lo,
                    dividend_hi,
                    dst_quotient,
                    dst_remainder,
                    size,
                    ..
                } => {
                    let dividend_lo = dividend_lo.to_reg();
                    let dividend_hi = dividend_hi.to_reg();
                    let dst_quotient = dst_quotient.to_reg().to_reg();
                    let dst_remainder = dst_remainder.to_reg().to_reg();
                    debug_assert_eq!(dividend_lo, regs::rax());
                    debug_assert_eq!(dividend_hi, regs::rdx());
                    debug_assert_eq!(dst_quotient, regs::rax());
                    debug_assert_eq!(dst_remainder, regs::rdx());
                    (regs::rdx(), *size)
                }
                Inst::CheckedSRemSeq8 { dividend, dst, .. } => {
                    let dividend = dividend.to_reg();
                    let dst = dst.to_reg().to_reg();
                    debug_assert_eq!(dividend, regs::rax());
                    debug_assert_eq!(dst, regs::rax());
                    (regs::rax(), OperandSize::Size8)
                }
                _ => unreachable!(),
            };

            // Generates the following code sequence:
            //
            // cmp -1 %divisor
            // jnz $do_op
            //
            // ;; for srem, result is 0
            // mov #0, %dst
            // j $done
            //
            // $do_op:
            // idiv %divisor
            //
            // $done:

            let do_op = sink.get_label();
            let done_label = sink.get_label();

            // Check if the divisor is -1, and if it isn't then immediately
            // go to the `idiv`.
            let inst = Inst::cmp_rmi_r(size, divisor.to_reg(), RegMemImm::imm(0xffffffff));
            inst.emit(sink, info, state);
            one_way_jmp(sink, CC::NZ, do_op);

            // ... otherwise the divisor is -1 and the result is always 0. This
            // is written to the destination register which will be %rax for
            // 8-bit srem and %rdx otherwise.
            //
            // Note that for 16-to-64-bit srem operations this leaves the
            // second destination, %rax, unchanged. This isn't semantically
            // correct if a lowering actually tries to use the `dst_quotient`
            // output but for srem only the `dst_remainder` output is used for
            // now.
            let inst = Inst::imm(OperandSize::Size64, 0, Writable::from_reg(dst));
            inst.emit(sink, info, state);
            let inst = Inst::jmp_known(done_label);
            inst.emit(sink, info, state);

            // Here the `idiv` is executed, which is different depending on the
            // size
            sink.bind_label(do_op, state.ctrl_plane_mut());
            let rax = Gpr::unwrap_new(regs::rax());
            let rdx = Gpr::unwrap_new(regs::rdx());
            let writable_rax = Writable::from_reg(rax);
            let writable_rdx = Writable::from_reg(rdx);
            let inst = match size {
                OperandSize::Size8 => asm::inst::idivb_m::new(
                    PairedGpr::from(writable_rax),
                    *divisor,
                    TrapCode::INTEGER_DIVISION_BY_ZERO,
                )
                .into(),

                OperandSize::Size16 => asm::inst::idivw_m::new(
                    PairedGpr::from(writable_rax),
                    PairedGpr::from(writable_rdx),
                    *divisor,
                    TrapCode::INTEGER_DIVISION_BY_ZERO,
                )
                .into(),

                OperandSize::Size32 => asm::inst::idivl_m::new(
                    PairedGpr::from(writable_rax),
                    PairedGpr::from(writable_rdx),
                    *divisor,
                    TrapCode::INTEGER_DIVISION_BY_ZERO,
                )
                .into(),

                OperandSize::Size64 => asm::inst::idivq_m::new(
                    PairedGpr::from(writable_rax),
                    PairedGpr::from(writable_rdx),
                    *divisor,
                    TrapCode::INTEGER_DIVISION_BY_ZERO,
                )
                .into(),
            };
            Inst::External { inst }.emit(sink, info, state);

            sink.bind_label(done_label, state.ctrl_plane_mut());
        }

        Inst::Imm {
            dst_size,
            simm64,
            dst,
        } => {
            let dst = dst.to_reg().to_reg();
            let enc_dst = int_reg_enc(dst);
            if *dst_size == OperandSize::Size64 {
                if low32_will_sign_extend_to_64(*simm64) {
                    // Sign-extended move imm32.
                    emit_std_enc_enc(
                        sink,
                        LegacyPrefixes::None,
                        0xC7,
                        1,
                        /* subopcode */ 0,
                        enc_dst,
                        RexFlags::set_w(),
                    );
                    sink.put4(*simm64 as u32);
                } else {
                    sink.put1(0x48 | ((enc_dst >> 3) & 1));
                    sink.put1(0xB8 | (enc_dst & 7));
                    sink.put8(*simm64);
                }
            } else {
                if ((enc_dst >> 3) & 1) == 1 {
                    sink.put1(0x41);
                }
                sink.put1(0xB8 | (enc_dst & 7));
                sink.put4(*simm64 as u32);
            }
        }

        Inst::MovImmM { size, simm32, dst } => {
            let dst = &dst.finalize(state.frame_layout(), sink).clone();
            let default_rex = RexFlags::clear_w();
            let default_opcode = 0xC7;
            let bytes = size.to_bytes();
            let prefix = LegacyPrefixes::None;

            let (opcode, rex, size, prefix) = match *size {
                // In the 8-bit case, we don't need to enforce REX flags via
                // `always_emit_if_8bit_needed()` since the destination
                // operand is a memory operand, not a possibly 8-bit register.
                OperandSize::Size8 => (0xC6, default_rex, bytes, prefix),
                OperandSize::Size16 => (0xC7, default_rex, bytes, LegacyPrefixes::_66),
                OperandSize::Size64 => (default_opcode, RexFlags::from(*size), bytes, prefix),

                _ => (default_opcode, default_rex, bytes, prefix),
            };

            // 8-bit C6 /0 ib
            // 16-bit 0x66 C7 /0 iw
            // 32-bit C7 /0 id
            // 64-bit REX.W C7 /0 id
            emit_std_enc_mem(sink, prefix, opcode, 1, /*subopcode*/ 0, dst, rex, 0);
            emit_simm(sink, size, *simm32 as u32);
        }

        Inst::MovRR { size, src, dst } => {
            let src = src.to_reg();
            let dst = dst.to_reg().to_reg();
            emit_std_reg_reg(
                sink,
                LegacyPrefixes::None,
                0x89,
                1,
                src,
                dst,
                RexFlags::from(*size),
            );
        }

        Inst::MovFromPReg { src, dst } => {
            let src: Reg = (*src).into();
            debug_assert!([regs::rsp(), regs::rbp(), regs::pinned_reg()].contains(&src));
            let src = Gpr::unwrap_new(src);
            let size = OperandSize::Size64;
            let dst = WritableGpr::from_writable_reg(dst.to_writable_reg()).unwrap();
            Inst::MovRR { size, src, dst }.emit(sink, info, state);
        }

        Inst::MovToPReg { src, dst } => {
            let src = src.to_reg();
            let src = Gpr::unwrap_new(src);
            let dst: Reg = (*dst).into();
            debug_assert!([regs::rsp(), regs::rbp(), regs::pinned_reg()].contains(&dst));
            let dst = WritableGpr::from_writable_reg(Writable::from_reg(dst)).unwrap();
            let size = OperandSize::Size64;
            Inst::MovRR { size, src, dst }.emit(sink, info, state);
        }

        Inst::LoadEffectiveAddress { addr, dst, size } => {
            let dst = dst.to_reg().to_reg();
            let amode = addr.finalize(state.frame_layout(), sink).clone();

            // If this `lea` can actually get encoded as an `add` then do that
            // instead. Currently all candidate `iadd`s become an `lea`
            // pseudo-instruction here but maximizing the use of `lea` is not
            // necessarily optimal. The `lea` instruction goes through dedicated
            // address units on cores which are finite and disjoint from the
            // general ALU, so if everything uses `lea` then those units can get
            // saturated while leaving the ALU idle.
            //
            // To help make use of more parts of a CPU, this attempts to use
            // `add` when it's semantically equivalent to `lea`, or otherwise
            // when the `dst` register is the same as the `base` or `index`
            // register.
            //
            // FIXME: ideally regalloc is informed of this constraint. Register
            // allocation of `lea` should "attempt" to put the `base` in the
            // same register as `dst` but not at the expense of generating a
            // `mov` instruction. Currently that's not possible but perhaps one
            // day it may be worth it.
            match amode {
                // If `base == dst` then this is `add $imm, %dst`, so encode
                // that instead.
                Amode::ImmReg {
                    simm32,
                    base,
                    flags: _,
                } if base == dst => {
                    let dst = Writable::from_reg(dst);
                    let inst = match size {
                        OperandSize::Size32 => Inst::External {
                            inst: asm::inst::addl_mi::new(dst, simm32 as u32).into(),
                        },
                        OperandSize::Size64 => Inst::addq_mi(dst, simm32),
                        _ => unreachable!(),
                    };
                    inst.emit(sink, info, state);
                }
                // If the offset is 0 and the shift is 0 (meaning multiplication
                // by 1) then:
                //
                // * If `base == dst`, then this is `add %index, %base`
                // * If `index == dst`, then this is `add %base, %index`
                //
                // Encode the appropriate instruction here in that case.
                Amode::ImmRegRegShift {
                    simm32: 0,
                    base,
                    index,
                    shift: 0,
                    flags: _,
                } if base == dst || index == dst => {
                    let (dst, operand) = if base == dst {
                        (base, index)
                    } else {
                        (index, base)
                    };
                    let dst = Writable::from_reg(dst);
                    let inst = match size {
                        OperandSize::Size32 => asm::inst::addl_rm::new(dst, operand).into(),
                        OperandSize::Size64 => asm::inst::addq_rm::new(dst, operand).into(),
                        _ => unreachable!(),
                    };
                    Inst::External { inst }.emit(sink, info, state);
                }

                // If `lea`'s 3-operand mode is leveraged by regalloc, or if
                // it's fancy like imm-plus-shift-plus-base, then `lea` is
                // actually emitted.
                _ => {
                    let flags = match size {
                        OperandSize::Size32 => RexFlags::clear_w(),
                        OperandSize::Size64 => RexFlags::set_w(),
                        _ => unreachable!(),
                    };
                    emit_std_reg_mem(sink, LegacyPrefixes::None, 0x8D, 1, dst, &amode, flags, 0);
                }
            };
        }

        Inst::MovRM { size, src, dst } => {
            let src = src.to_reg();
            let dst = &dst.finalize(state.frame_layout(), sink).clone();

            let prefix = match size {
                OperandSize::Size16 => LegacyPrefixes::_66,
                _ => LegacyPrefixes::None,
            };

            let opcode = match size {
                OperandSize::Size8 => 0x88,
                _ => 0x89,
            };

            // This is one of the few places where the presence of a
            // redundant REX prefix changes the meaning of the
            // instruction.
            let rex = RexFlags::from((*size, src));

            //  8-bit: MOV r8, r/m8 is (REX.W==0) 88 /r
            // 16-bit: MOV r16, r/m16 is 66 (REX.W==0) 89 /r
            // 32-bit: MOV r32, r/m32 is (REX.W==0) 89 /r
            // 64-bit: MOV r64, r/m64 is (REX.W==1) 89 /r
            emit_std_reg_mem(sink, prefix, opcode, 1, src, dst, rex, 0);
        }

        Inst::CmpRmiR {
            size,
            src1: reg_g,
            src2: src_e,
            opcode,
        } => {
            let reg_g = reg_g.to_reg();

            let is_cmp = match opcode {
                CmpOpcode::Cmp => true,
                CmpOpcode::Test => false,
            };

            let mut prefix = LegacyPrefixes::None;
            if *size == OperandSize::Size16 {
                prefix = LegacyPrefixes::_66;
            }
            // A redundant REX prefix can change the meaning of this instruction.
            let mut rex = RexFlags::from((*size, reg_g));

            match src_e.clone().to_reg_mem_imm() {
                RegMemImm::Reg { reg: reg_e } => {
                    if *size == OperandSize::Size8 {
                        // Check whether the E register forces the use of a redundant REX.
                        rex.always_emit_if_8bit_needed(reg_e);
                    }

                    // Use the swapped operands encoding for CMP, to stay consistent with the output of
                    // gcc/llvm.
                    let opcode = match (*size, is_cmp) {
                        (OperandSize::Size8, true) => 0x38,
                        (_, true) => 0x39,
                        (OperandSize::Size8, false) => 0x84,
                        (_, false) => 0x85,
                    };
                    emit_std_reg_reg(sink, prefix, opcode, 1, reg_e, reg_g, rex);
                }

                RegMemImm::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink).clone();
                    // Whereas here we revert to the "normal" G-E ordering for CMP.
                    let opcode = match (*size, is_cmp) {
                        (OperandSize::Size8, true) => 0x3A,
                        (_, true) => 0x3B,
                        (OperandSize::Size8, false) => 0x84,
                        (_, false) => 0x85,
                    };
                    emit_std_reg_mem(sink, prefix, opcode, 1, reg_g, addr, rex, 0);
                }

                RegMemImm::Imm { simm32 } => {
                    // FIXME JRS 2020Feb11: there are shorter encodings for
                    // cmp $imm, rax/eax/ax/al.
                    let use_imm8 = is_cmp && low8_will_sign_extend_to_32(simm32);

                    // And also here we use the "normal" G-E ordering.
                    let opcode = if is_cmp {
                        if *size == OperandSize::Size8 {
                            0x80
                        } else if use_imm8 {
                            0x83
                        } else {
                            0x81
                        }
                    } else {
                        if *size == OperandSize::Size8 {
                            0xF6
                        } else {
                            0xF7
                        }
                    };
                    let subopcode = if is_cmp { 7 } else { 0 };

                    let enc_g = int_reg_enc(reg_g);
                    emit_std_enc_enc(sink, prefix, opcode, 1, subopcode, enc_g, rex);
                    emit_simm(sink, if use_imm8 { 1 } else { size.to_bytes() }, simm32);
                }
            }
        }

        Inst::Setcc { cc, dst } => {
            let dst = dst.to_reg().to_reg();
            let opcode = 0x0f90 + cc.get_enc() as u32;
            let mut rex_flags = RexFlags::clear_w();
            rex_flags.always_emit();
            emit_std_enc_enc(
                sink,
                LegacyPrefixes::None,
                opcode,
                2,
                0,
                reg_enc(dst),
                rex_flags,
            );
        }

        Inst::Cmove {
            size,
            cc,
            consequent,
            alternative,
            dst,
        } => {
            let alternative = alternative.to_reg();
            let dst = dst.to_reg().to_reg();
            debug_assert_eq!(alternative, dst);
            let rex_flags = RexFlags::from(*size);
            let prefix = match size {
                OperandSize::Size16 => LegacyPrefixes::_66,
                OperandSize::Size32 => LegacyPrefixes::None,
                OperandSize::Size64 => LegacyPrefixes::None,
                _ => unreachable!("invalid size spec for cmove"),
            };
            let opcode = 0x0F40 + cc.get_enc() as u32;
            match consequent.clone().to_reg_mem() {
                RegMem::Reg { reg } => {
                    emit_std_reg_reg(sink, prefix, opcode, 2, dst, reg, rex_flags);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink).clone();
                    emit_std_reg_mem(sink, prefix, opcode, 2, dst, addr, rex_flags, 0);
                }
            }
        }

        Inst::XmmCmove {
            ty,
            cc,
            consequent,
            alternative,
            dst,
        } => {
            let alternative = *alternative;
            let dst = *dst;
            debug_assert_eq!(alternative, dst.to_reg());
            let consequent = *consequent;

            // Lowering of the Select IR opcode when the input is an fcmp relies on the fact that
            // this doesn't clobber flags. Make sure to not do so here.
            let next = sink.get_label();

            // Jump if cc is *not* set.
            one_way_jmp(sink, cc.invert(), next);
            Inst::gen_move(dst.map(|r| r.to_reg()), consequent.to_reg(), *ty)
                .emit(sink, info, state);

            sink.bind_label(next, state.ctrl_plane_mut());
        }

        Inst::StackProbeLoop {
            tmp,
            frame_size,
            guard_size,
        } => {
            assert!(info.flags.enable_probestack());
            assert!(guard_size.is_power_of_two());

            let tmp = *tmp;

            // Number of probes that we need to perform
            let probe_count = align_to(*frame_size, *guard_size) / guard_size;

            // The inline stack probe loop has 3 phases:
            //
            // We generate the "guard area" register which is essentially the frame_size aligned to
            // guard_size. We copy the stack pointer and subtract the guard area from it. This
            // gets us a register that we can use to compare when looping.
            //
            // After that we emit the loop. Essentially we just adjust the stack pointer one guard_size'd
            // distance at a time and then touch the stack by writing anything to it. We use the previously
            // created "guard area" register to know when to stop looping.
            //
            // When we have touched all the pages that we need, we have to restore the stack pointer
            // to where it was before.
            //
            // Generate the following code:
            //         mov  tmp_reg, rsp
            //         sub  tmp_reg, guard_size * probe_count
            // .loop_start:
            //         sub  rsp, guard_size
            //         mov  [rsp], rsp
            //         cmp  rsp, tmp_reg
            //         jne  .loop_start
            //         add  rsp, guard_size * probe_count

            // Create the guard bound register
            // mov  tmp_reg, rsp
            let inst = Inst::gen_move(tmp, regs::rsp(), types::I64);
            inst.emit(sink, info, state);

            // sub  tmp_reg, GUARD_SIZE * probe_count
            let guard_plus_count = i32::try_from(guard_size * probe_count)
                .expect("`guard_size * probe_count` is too large to fit in a 32-bit immediate");
            Inst::subq_mi(tmp, guard_plus_count).emit(sink, info, state);

            // Emit the main loop!
            let loop_start = sink.get_label();
            sink.bind_label(loop_start, state.ctrl_plane_mut());

            // sub  rsp, GUARD_SIZE
            let rsp = Writable::from_reg(regs::rsp());
            let guard_size_ = i32::try_from(*guard_size)
                .expect("`guard_size` is too large to fit in a 32-bit immediate");
            Inst::subq_mi(rsp, guard_size_).emit(sink, info, state);

            // TODO: `mov [rsp], 0` would be better, but we don't have that instruction
            // Probe the stack! We don't use Inst::gen_store_stack here because we need a predictable
            // instruction size.
            // mov  [rsp], rsp
            let inst = Inst::mov_r_m(
                OperandSize::Size32, // Use Size32 since it saves us one byte
                regs::rsp(),
                SyntheticAmode::Real(Amode::imm_reg(0, regs::rsp())),
            );
            inst.emit(sink, info, state);

            // Compare and jump if we are not done yet
            // cmp  rsp, tmp_reg
            let inst = Inst::cmp_rmi_r(
                OperandSize::Size64,
                tmp.to_reg(),
                RegMemImm::reg(regs::rsp()),
            );
            inst.emit(sink, info, state);

            // jne  .loop_start
            // TODO: Encoding the conditional jump as a short jump
            // could save us us 4 bytes here.
            one_way_jmp(sink, CC::NZ, loop_start);

            // The regular prologue code is going to emit a `sub` after this, so we need to
            // reset the stack pointer
            //
            // TODO: It would be better if we could avoid the `add` + `sub` that is generated here
            // and in the stack adj portion of the prologue
            //
            // add rsp, GUARD_SIZE * probe_count
            Inst::addq_mi(rsp, guard_plus_count).emit(sink, info, state);
        }

        Inst::CallKnown { info: call_info } => {
            if let Some(s) = state.take_stack_map() {
                let offset = sink.cur_offset() + 5;
                sink.push_user_stack_map(state, offset, s);
            }

            sink.put1(0xE8);
            // The addend adjusts for the difference between the end of the
            // instruction and the beginning of the immediate field.
            emit_reloc(sink, Reloc::X86CallPCRel4, &call_info.dest, -4);
            sink.put4(0);

            if let Some(try_call) = call_info.try_call_info.as_ref() {
                sink.add_call_site(&try_call.exception_dests);
            } else {
                sink.add_call_site(&[]);
            }

            // Reclaim the outgoing argument area that was released by the
            // callee, to ensure that StackAMode values are always computed from
            // a consistent SP.
            if call_info.callee_pop_size > 0 {
                let rsp = Writable::from_reg(regs::rsp());
                let callee_pop_size = i32::try_from(call_info.callee_pop_size)
                    .expect("`callee_pop_size` is too large to fit in a 32-bit immediate");
                Inst::subq_mi(rsp, callee_pop_size).emit(sink, info, state);
            }

            // Load any stack-carried return values.
            call_info.emit_retval_loads::<X64ABIMachineSpec, _, _>(
                state.frame_layout().stackslots_size,
                |inst| inst.emit(sink, info, state),
                |_space_needed| None,
            );

            // If this is a try-call, jump to the continuation
            // (normal-return) block.
            if let Some(try_call) = call_info.try_call_info.as_ref() {
                let jmp = Inst::JmpKnown {
                    dst: try_call.continuation,
                };
                jmp.emit(sink, info, state);
            }
        }

        Inst::ReturnCallKnown { info: call_info } => {
            emit_return_call_common_sequence(sink, info, state, &call_info);

            // Finally, jump to the callee!
            //
            // Note: this is not `Inst::Jmp { .. }.emit(..)` because we have
            // different metadata in this case: we don't have a label for the
            // target, but rather a function relocation.
            sink.put1(0xE9);
            // The addend adjusts for the difference between the end of the instruction and the
            // beginning of the immediate field.
            emit_reloc(sink, Reloc::X86CallPCRel4, &call_info.dest, -4);
            sink.put4(0);
            sink.add_call_site(&[]);
        }

        Inst::ReturnCallUnknown { info: call_info } => {
            let callee = call_info.dest;

            emit_return_call_common_sequence(sink, info, state, &call_info);

            Inst::JmpUnknown {
                target: RegMem::reg(callee),
            }
            .emit(sink, info, state);
            sink.add_call_site(&[]);
        }

        Inst::CallUnknown {
            info: call_info, ..
        } => {
            let dest = call_info.dest.clone();

            match dest {
                RegMem::Reg { reg } => {
                    let reg_enc = int_reg_enc(reg);
                    emit_std_enc_enc(
                        sink,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        2, /*subopcode*/
                        reg_enc,
                        RexFlags::clear_w(),
                    );
                }

                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink);
                    emit_std_enc_mem(
                        sink,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        2, /*subopcode*/
                        addr,
                        RexFlags::clear_w(),
                        0,
                    );
                }
            }

            if let Some(s) = state.take_stack_map() {
                let offset = sink.cur_offset();
                sink.push_user_stack_map(state, offset, s);
            }

            if let Some(try_call) = call_info.try_call_info.as_ref() {
                sink.add_call_site(&try_call.exception_dests);
            } else {
                sink.add_call_site(&[]);
            }

            // Reclaim the outgoing argument area that was released by the callee, to ensure that
            // StackAMode values are always computed from a consistent SP.
            if call_info.callee_pop_size > 0 {
                let rsp = Writable::from_reg(regs::rsp());
                let callee_pop_size = i32::try_from(call_info.callee_pop_size)
                    .expect("`callee_pop_size` is too large to fit in a 32-bit immediate");
                Inst::subq_mi(rsp, callee_pop_size).emit(sink, info, state);
            }

            // Load any stack-carried return values.
            call_info.emit_retval_loads::<X64ABIMachineSpec, _, _>(
                state.frame_layout().stackslots_size,
                |inst| inst.emit(sink, info, state),
                |_space_needed| None,
            );

            if let Some(try_call) = call_info.try_call_info.as_ref() {
                let jmp = Inst::JmpKnown {
                    dst: try_call.continuation,
                };
                jmp.emit(sink, info, state);
            }
        }

        Inst::Args { .. } => {}
        Inst::Rets { .. } => {}

        Inst::Ret {
            stack_bytes_to_pop: 0,
        } => sink.put1(0xC3),

        Inst::Ret { stack_bytes_to_pop } => {
            sink.put1(0xC2);
            sink.put2(u16::try_from(*stack_bytes_to_pop).unwrap());
        }

        Inst::StackSwitchBasic {
            store_context_ptr,
            load_context_ptr,
            in_payload0,
            out_payload0,
        } => {
            // Note that we do not emit anything for preserving and restoring
            // ordinary registers here: That's taken care of by regalloc for us,
            // since we marked this instruction as clobbering all registers.
            //
            // Also note that we do nothing about passing the single payload
            // value: We've informed regalloc that it is sent and received via
            // the fixed register given by [stack_switch::payload_register]

            let (tmp1, tmp2) = {
                // Ideally we would just ask regalloc for two temporary registers.
                // However, adding any early defs to the constraints on StackSwitch
                // causes TooManyLiveRegs. Fortunately, we can manually find tmp
                // registers without regalloc: Since our instruction clobbers all
                // registers, we can simply pick any register that is not assigned
                // to the operands.

                let all = crate::isa::x64::abi::ALL_CLOBBERS;

                let used_regs = [
                    **load_context_ptr,
                    **store_context_ptr,
                    **in_payload0,
                    *out_payload0.to_reg(),
                ];

                let mut tmps = all.into_iter().filter_map(|preg| {
                    let reg: Reg = preg.into();
                    if !used_regs.contains(&reg) {
                        WritableGpr::from_writable_reg(isle::WritableReg::from_reg(reg))
                    } else {
                        None
                    }
                });
                (tmps.next().unwrap(), tmps.next().unwrap())
            };

            let layout = stack_switch::control_context_layout();
            let rsp_offset = layout.stack_pointer_offset as i32;
            let pc_offset = layout.ip_offset as i32;
            let rbp_offset = layout.frame_pointer_offset as i32;

            // Location to which someone switch-ing back to this stack will jump
            // to: Right behind the `StackSwitch` instruction
            let resume = sink.get_label();

            //
            // For RBP and RSP we do the following:
            // - Load new value for register from `load_context_ptr` +
            // corresponding offset.
            // - Store previous (!) value of register at `store_context_ptr` +
            // corresponding offset.
            //
            // Since `load_context_ptr` and `store_context_ptr` are allowed to be
            // equal, we need to use a temporary register here.
            //

            let mut exchange = |offset, reg| {
                let addr = SyntheticAmode::real(Amode::imm_reg(offset, **load_context_ptr));
                let inst = asm::inst::movq_rm::new(tmp1, addr).into();
                Inst::External { inst }.emit(sink, info, state);

                let inst = Inst::MovRM {
                    size: OperandSize::Size64,
                    src: Gpr::new(reg).unwrap(),
                    dst: Amode::imm_reg(offset, **store_context_ptr).into(),
                };
                emit(&inst, sink, info, state);

                let dst = Writable::from_reg(reg);
                let inst = Inst::MovRR {
                    size: OperandSize::Size64,
                    src: tmp1.to_reg(),
                    dst: WritableGpr::from_writable_reg(dst).unwrap(),
                };
                emit(&inst, sink, info, state);
            };

            exchange(rsp_offset, regs::rsp());
            exchange(rbp_offset, regs::rbp());

            //
            // Load target PC, store resume PC, jump to target PC
            //

            let addr = SyntheticAmode::real(Amode::imm_reg(pc_offset, **load_context_ptr));
            let inst = asm::inst::movq_rm::new(tmp1, addr).into();
            Inst::External { inst }.emit(sink, info, state);

            let amode = Amode::RipRelative { target: resume };
            let inst = Inst::lea(amode, tmp2.map(Reg::from));
            inst.emit(sink, info, state);

            let inst = Inst::MovRM {
                size: OperandSize::Size64,
                src: tmp2.to_reg(),
                dst: Amode::imm_reg(pc_offset, **store_context_ptr).into(),
            };
            emit(&inst, sink, info, state);

            let inst = Inst::JmpUnknown {
                target: RegMem::reg(tmp1.to_reg().into()),
            };
            emit(&inst, sink, info, state);

            sink.bind_label(resume, state.ctrl_plane_mut());
        }

        Inst::JmpKnown { dst } => {
            let br_start = sink.cur_offset();
            let br_disp_off = br_start + 1;
            let br_end = br_start + 5;

            sink.use_label_at_offset(br_disp_off, *dst, LabelUse::JmpRel32);
            sink.add_uncond_branch(br_start, br_end, *dst);

            sink.put1(0xE9);
            // Placeholder for the label value.
            sink.put4(0x0);
        }

        Inst::WinchJmpIf { cc, taken } => {
            let cond_start = sink.cur_offset();
            let cond_disp_off = cond_start + 2;

            sink.use_label_at_offset(cond_disp_off, *taken, LabelUse::JmpRel32);
            // Since this is not a terminator, don't enroll in the branch inversion mechanism.

            sink.put1(0x0F);
            sink.put1(0x80 + cc.get_enc());
            // Placeholder for the label value.
            sink.put4(0x0);
        }

        Inst::JmpCond {
            cc,
            taken,
            not_taken,
        } => {
            // If taken.
            let cond_start = sink.cur_offset();
            let cond_disp_off = cond_start + 2;
            let cond_end = cond_start + 6;

            sink.use_label_at_offset(cond_disp_off, *taken, LabelUse::JmpRel32);
            let inverted: [u8; 6] = [0x0F, 0x80 + (cc.invert().get_enc()), 0x00, 0x00, 0x00, 0x00];
            sink.add_cond_branch(cond_start, cond_end, *taken, &inverted[..]);

            sink.put1(0x0F);
            sink.put1(0x80 + cc.get_enc());
            // Placeholder for the label value.
            sink.put4(0x0);

            // If not taken.
            let uncond_start = sink.cur_offset();
            let uncond_disp_off = uncond_start + 1;
            let uncond_end = uncond_start + 5;

            sink.use_label_at_offset(uncond_disp_off, *not_taken, LabelUse::JmpRel32);
            sink.add_uncond_branch(uncond_start, uncond_end, *not_taken);

            sink.put1(0xE9);
            // Placeholder for the label value.
            sink.put4(0x0);
        }

        Inst::JmpCondOr {
            cc1,
            cc2,
            taken,
            not_taken,
        } => {
            // Emit:
            //   jcc1 taken
            //   jcc2 taken
            //   jmp not_taken
            //
            // Note that we enroll both conditionals in the
            // branch-chomping mechanism because MachBuffer
            // simplification can continue upward as long as it keeps
            // chomping branches. In the best case, if taken ==
            // not_taken and that one block is the fallthrough block,
            // all three branches can disappear.

            // jcc1 taken
            let cond_1_start = sink.cur_offset();
            let cond_1_disp_off = cond_1_start + 2;
            let cond_1_end = cond_1_start + 6;

            sink.use_label_at_offset(cond_1_disp_off, *taken, LabelUse::JmpRel32);
            let inverted: [u8; 6] = [
                0x0F,
                0x80 + (cc1.invert().get_enc()),
                0x00,
                0x00,
                0x00,
                0x00,
            ];
            sink.add_cond_branch(cond_1_start, cond_1_end, *taken, &inverted[..]);

            sink.put1(0x0F);
            sink.put1(0x80 + cc1.get_enc());
            sink.put4(0x0);

            // jcc2 taken
            let cond_2_start = sink.cur_offset();
            let cond_2_disp_off = cond_2_start + 2;
            let cond_2_end = cond_2_start + 6;

            sink.use_label_at_offset(cond_2_disp_off, *taken, LabelUse::JmpRel32);
            let inverted: [u8; 6] = [
                0x0F,
                0x80 + (cc2.invert().get_enc()),
                0x00,
                0x00,
                0x00,
                0x00,
            ];
            sink.add_cond_branch(cond_2_start, cond_2_end, *taken, &inverted[..]);

            sink.put1(0x0F);
            sink.put1(0x80 + cc2.get_enc());
            sink.put4(0x0);

            // jmp not_taken
            let uncond_start = sink.cur_offset();
            let uncond_disp_off = uncond_start + 1;
            let uncond_end = uncond_start + 5;

            sink.use_label_at_offset(uncond_disp_off, *not_taken, LabelUse::JmpRel32);
            sink.add_uncond_branch(uncond_start, uncond_end, *not_taken);

            sink.put1(0xE9);
            sink.put4(0x0);
        }

        Inst::JmpUnknown { target } => {
            let target = target.clone();

            match target {
                RegMem::Reg { reg } => {
                    let reg_enc = int_reg_enc(reg);
                    emit_std_enc_enc(
                        sink,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        4, /*subopcode*/
                        reg_enc,
                        RexFlags::clear_w(),
                    );
                }

                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink);
                    emit_std_enc_mem(
                        sink,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        4, /*subopcode*/
                        addr,
                        RexFlags::clear_w(),
                        0,
                    );
                }
            }
        }

        &Inst::JmpTableSeq {
            idx,
            tmp1,
            tmp2,
            ref targets,
            ref default_target,
            ..
        } => {
            // This sequence is *one* instruction in the vcode, and is expanded only here at
            // emission time, because we cannot allow the regalloc to insert spills/reloads in
            // the middle; we depend on hardcoded PC-rel addressing below.
            //
            // We don't have to worry about emitting islands, because the only label-use type has a
            // maximum range of 2 GB. If we later consider using shorter-range label references,
            // this will need to be revisited.

            // We generate the following sequence. Note that the only read of %idx is before the
            // write to %tmp2, so regalloc may use the same register for both; fix x64/inst/mod.rs
            // if you change this.
            // lea start_of_jump_table_offset(%rip), %tmp1
            // movslq [%tmp1, %idx, 4], %tmp2 ;; shift of 2, viz. multiply index by 4
            // addq %tmp2, %tmp1
            // j *%tmp1
            // $start_of_jump_table:
            // -- jump table entries

            // Load base address of jump table.
            let start_of_jumptable = sink.get_label();
            let inst = Inst::lea(Amode::rip_relative(start_of_jumptable), tmp1);
            inst.emit(sink, info, state);

            // Load value out of the jump table. It's a relative offset to the target block, so it
            // might be negative; use a sign-extension.
            let inst = Inst::movsx_rm_r(
                ExtMode::LQ,
                RegMem::mem(Amode::imm_reg_reg_shift(
                    0,
                    Gpr::unwrap_new(tmp1.to_reg()),
                    Gpr::unwrap_new(idx),
                    2,
                )),
                tmp2,
            );
            inst.emit(sink, info, state);

            // Add base of jump table to jump-table-sourced block offset.
            let inst = Inst::External {
                inst: asm::inst::addq_rm::new(tmp1, tmp2).into(),
            };
            inst.emit(sink, info, state);

            // Branch to computed address.
            let inst = Inst::jmp_unknown(RegMem::reg(tmp1.to_reg()));
            inst.emit(sink, info, state);

            // Emit jump table (table of 32-bit offsets).
            sink.bind_label(start_of_jumptable, state.ctrl_plane_mut());
            let jt_off = sink.cur_offset();
            for &target in targets.iter().chain(std::iter::once(default_target)) {
                let word_off = sink.cur_offset();
                // off_into_table is an addend here embedded in the label to be later patched at
                // the end of codegen. The offset is initially relative to this jump table entry;
                // with the extra addend, it'll be relative to the jump table's start, after
                // patching.
                let off_into_table = word_off - jt_off;
                sink.use_label_at_offset(word_off, target, LabelUse::PCRel32);
                sink.put4(off_into_table);
            }
        }

        Inst::TrapIf { cc, trap_code } => {
            let trap_label = sink.defer_trap(*trap_code);
            one_way_jmp(sink, *cc, trap_label);
        }

        Inst::TrapIfAnd {
            cc1,
            cc2,
            trap_code,
        } => {
            let trap_label = sink.defer_trap(*trap_code);
            let else_label = sink.get_label();

            // Jump to the end if the first condition isn't true, and then if
            // the second condition is true go to the trap.
            one_way_jmp(sink, cc1.invert(), else_label);
            one_way_jmp(sink, *cc2, trap_label);

            sink.bind_label(else_label, state.ctrl_plane_mut());
        }

        Inst::TrapIfOr {
            cc1,
            cc2,
            trap_code,
        } => {
            let trap_label = sink.defer_trap(*trap_code);

            // Emit two jumps to the same trap if either condition code is true.
            one_way_jmp(sink, *cc1, trap_label);
            one_way_jmp(sink, *cc2, trap_label);
        }

        Inst::XmmUnaryRmR {
            op,
            src: src_e,
            dst: reg_g,
        } => {
            let reg_g = reg_g.to_reg().to_reg();
            let src_e = src_e.clone().to_reg_mem().clone();

            let rex = RexFlags::clear_w();

            let (prefix, opcode, num_opcodes) = match op {
                SseOpcode::Pabsb => (LegacyPrefixes::_66, 0x0F381C, 3),
                SseOpcode::Pabsw => (LegacyPrefixes::_66, 0x0F381D, 3),
                SseOpcode::Pabsd => (LegacyPrefixes::_66, 0x0F381E, 3),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };

            match src_e {
                RegMem::Reg { reg: reg_e } => {
                    emit_std_reg_reg(sink, prefix, opcode, num_opcodes, reg_g, reg_e, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink);
                    emit_std_reg_mem(sink, prefix, opcode, num_opcodes, reg_g, addr, rex, 0);
                }
            };
        }

        Inst::XmmUnaryRmREvex { op, src, dst } => {
            let dst = dst.to_reg().to_reg();
            let src = match src.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (prefix, map, w, opcode) = match op {
                Avx512Opcode::Vcvtudq2ps => (LegacyPrefixes::_F2, OpcodeMap::_0F, false, 0x7a),
                Avx512Opcode::Vpabsq => (LegacyPrefixes::_66, OpcodeMap::_0F38, true, 0x1f),
                Avx512Opcode::Vpopcntb => (LegacyPrefixes::_66, OpcodeMap::_0F38, false, 0x54),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            EvexInstruction::new()
                .length(EvexVectorLength::V128)
                .prefix(prefix)
                .map(map)
                .w(w)
                .opcode(opcode)
                .tuple_type(op.tuple_type())
                .reg(dst.to_real_reg().unwrap().hw_enc())
                .rm(src)
                .encode(sink);
        }

        Inst::XmmUnaryRmRImmEvex { op, src, dst, imm } => {
            let dst = dst.to_reg().to_reg();
            let src = match src.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (opcode, opcode_ext, w) = match op {
                Avx512Opcode::VpsraqImm => (0x72, 4, true),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            EvexInstruction::new()
                .length(EvexVectorLength::V128)
                .prefix(LegacyPrefixes::_66)
                .map(OpcodeMap::_0F)
                .w(w)
                .opcode(opcode)
                .reg(opcode_ext)
                .vvvvv(dst.to_real_reg().unwrap().hw_enc())
                .tuple_type(op.tuple_type())
                .rm(src)
                .imm(*imm)
                .encode(sink);
        }

        Inst::XmmRmR {
            op,
            src1,
            src2,
            dst,
        } => emit(
            &Inst::XmmRmRUnaligned {
                op: *op,
                dst: *dst,
                src1: *src1,
                src2: XmmMem::unwrap_new(src2.clone().to_reg_mem()),
            },
            sink,
            info,
            state,
        ),

        Inst::XmmRmRUnaligned {
            op,
            src1,
            src2: src_e,
            dst: reg_g,
        } => {
            let src1 = src1.to_reg();
            let reg_g = reg_g.to_reg().to_reg();
            let src_e = src_e.clone().to_reg_mem().clone();
            debug_assert_eq!(src1, reg_g);

            let rex = RexFlags::clear_w();
            let (prefix, opcode, length) = match op {
                SseOpcode::Movlhps => (LegacyPrefixes::None, 0x0F16, 2),
                SseOpcode::Packssdw => (LegacyPrefixes::_66, 0x0F6B, 2),
                SseOpcode::Packsswb => (LegacyPrefixes::_66, 0x0F63, 2),
                SseOpcode::Packusdw => (LegacyPrefixes::_66, 0x0F382B, 3),
                SseOpcode::Packuswb => (LegacyPrefixes::_66, 0x0F67, 2),
                SseOpcode::Pmaddubsw => (LegacyPrefixes::_66, 0x0F3804, 3),
                SseOpcode::Pavgb => (LegacyPrefixes::_66, 0x0FE0, 2),
                SseOpcode::Pavgw => (LegacyPrefixes::_66, 0x0FE3, 2),
                SseOpcode::Pcmpeqb => (LegacyPrefixes::_66, 0x0F74, 2),
                SseOpcode::Pcmpeqw => (LegacyPrefixes::_66, 0x0F75, 2),
                SseOpcode::Pcmpeqd => (LegacyPrefixes::_66, 0x0F76, 2),
                SseOpcode::Pcmpeqq => (LegacyPrefixes::_66, 0x0F3829, 3),
                SseOpcode::Pcmpgtb => (LegacyPrefixes::_66, 0x0F64, 2),
                SseOpcode::Pcmpgtw => (LegacyPrefixes::_66, 0x0F65, 2),
                SseOpcode::Pcmpgtd => (LegacyPrefixes::_66, 0x0F66, 2),
                SseOpcode::Pcmpgtq => (LegacyPrefixes::_66, 0x0F3837, 3),
                SseOpcode::Pmaddwd => (LegacyPrefixes::_66, 0x0FF5, 2),
                SseOpcode::Pshufb => (LegacyPrefixes::_66, 0x0F3800, 3),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };

            match src_e {
                RegMem::Reg { reg: reg_e } => {
                    emit_std_reg_reg(sink, prefix, opcode, length, reg_g, reg_e, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink);
                    emit_std_reg_mem(sink, prefix, opcode, length, reg_g, addr, rex, 0);
                }
            }
        }

        Inst::XmmRmiRVex {
            op,
            src1,
            src2,
            dst,
        } => {
            use LegacyPrefixes as LP;
            use OpcodeMap as OM;

            let dst = dst.to_reg().to_reg();
            let src1 = src1.to_reg();
            let src2 = src2.clone().to_reg_mem_imm().clone();

            // When the opcode is commutative, src1 is xmm{0..7}, and src2 is
            // xmm{8..15}, then we can swap the operands to save one byte on the
            // instruction's encoding.
            let (src1, src2) = match (src1, src2) {
                (src1, RegMemImm::Reg { reg: src2 })
                    if op.is_commutative()
                        && src1.to_real_reg().unwrap().hw_enc() < 8
                        && src2.to_real_reg().unwrap().hw_enc() >= 8 =>
                {
                    (src2, RegMemImm::Reg { reg: src1 })
                }
                (src1, src2) => (src1, src2),
            };

            let src2 = match src2 {
                // For opcodes where one of the operands is an immediate the
                // encoding is a bit different, notably the usage of
                // `opcode_ext`, so handle that specially here.
                RegMemImm::Imm { simm32 } => {
                    let (opcode, opcode_ext, prefix) = match op {
                        AvxOpcode::Vpsrlw => (0x71, 2, LegacyPrefixes::_66),
                        AvxOpcode::Vpsrld => (0x72, 2, LegacyPrefixes::_66),
                        AvxOpcode::Vpsrlq => (0x73, 2, LegacyPrefixes::_66),
                        AvxOpcode::Vpsllw => (0x71, 6, LegacyPrefixes::_66),
                        AvxOpcode::Vpslld => (0x72, 6, LegacyPrefixes::_66),
                        AvxOpcode::Vpsllq => (0x73, 6, LegacyPrefixes::_66),
                        AvxOpcode::Vpsraw => (0x71, 4, LegacyPrefixes::_66),
                        AvxOpcode::Vpsrad => (0x72, 4, LegacyPrefixes::_66),
                        _ => panic!("unexpected rmi_r_vex opcode with immediate {op:?}"),
                    };
                    VexInstruction::new()
                        .length(VexVectorLength::V128)
                        .prefix(prefix)
                        .map(OpcodeMap::_0F)
                        .opcode(opcode)
                        .opcode_ext(opcode_ext)
                        .vvvv(dst.to_real_reg().unwrap().hw_enc())
                        .prefix(LegacyPrefixes::_66)
                        .rm(src1.to_real_reg().unwrap().hw_enc())
                        .imm(simm32.try_into().unwrap())
                        .encode(sink);
                    return;
                }
                RegMemImm::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMemImm::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (prefix, map, opcode) = match op {
                AvxOpcode::Vminps => (LP::None, OM::_0F, 0x5D),
                AvxOpcode::Vminpd => (LP::_66, OM::_0F, 0x5D),
                AvxOpcode::Vmaxps => (LP::None, OM::_0F, 0x5F),
                AvxOpcode::Vmaxpd => (LP::_66, OM::_0F, 0x5F),
                AvxOpcode::Vandnps => (LP::None, OM::_0F, 0x55),
                AvxOpcode::Vandnpd => (LP::_66, OM::_0F, 0x55),
                AvxOpcode::Vpandn => (LP::_66, OM::_0F, 0xDF),
                AvxOpcode::Vpsrlw => (LP::_66, OM::_0F, 0xD1),
                AvxOpcode::Vpsrld => (LP::_66, OM::_0F, 0xD2),
                AvxOpcode::Vpsrlq => (LP::_66, OM::_0F, 0xD3),
                AvxOpcode::Vpaddb => (LP::_66, OM::_0F, 0xFC),
                AvxOpcode::Vpaddw => (LP::_66, OM::_0F, 0xFD),
                AvxOpcode::Vpaddd => (LP::_66, OM::_0F, 0xFE),
                AvxOpcode::Vpaddq => (LP::_66, OM::_0F, 0xD4),
                AvxOpcode::Vpaddsb => (LP::_66, OM::_0F, 0xEC),
                AvxOpcode::Vpaddsw => (LP::_66, OM::_0F, 0xED),
                AvxOpcode::Vpaddusb => (LP::_66, OM::_0F, 0xDC),
                AvxOpcode::Vpaddusw => (LP::_66, OM::_0F, 0xDD),
                AvxOpcode::Vpsubb => (LP::_66, OM::_0F, 0xF8),
                AvxOpcode::Vpsubw => (LP::_66, OM::_0F, 0xF9),
                AvxOpcode::Vpsubd => (LP::_66, OM::_0F, 0xFA),
                AvxOpcode::Vpsubq => (LP::_66, OM::_0F, 0xFB),
                AvxOpcode::Vpsubsb => (LP::_66, OM::_0F, 0xE8),
                AvxOpcode::Vpsubsw => (LP::_66, OM::_0F, 0xE9),
                AvxOpcode::Vpsubusb => (LP::_66, OM::_0F, 0xD8),
                AvxOpcode::Vpsubusw => (LP::_66, OM::_0F, 0xD9),
                AvxOpcode::Vpavgb => (LP::_66, OM::_0F, 0xE0),
                AvxOpcode::Vpavgw => (LP::_66, OM::_0F, 0xE3),
                AvxOpcode::Vpand => (LP::_66, OM::_0F, 0xDB),
                AvxOpcode::Vandps => (LP::None, OM::_0F, 0x54),
                AvxOpcode::Vandpd => (LP::_66, OM::_0F, 0x54),
                AvxOpcode::Vpor => (LP::_66, OM::_0F, 0xEB),
                AvxOpcode::Vorps => (LP::None, OM::_0F, 0x56),
                AvxOpcode::Vorpd => (LP::_66, OM::_0F, 0x56),
                AvxOpcode::Vpxor => (LP::_66, OM::_0F, 0xEF),
                AvxOpcode::Vxorps => (LP::None, OM::_0F, 0x57),
                AvxOpcode::Vxorpd => (LP::_66, OM::_0F, 0x57),
                AvxOpcode::Vpmullw => (LP::_66, OM::_0F, 0xD5),
                AvxOpcode::Vpmulld => (LP::_66, OM::_0F38, 0x40),
                AvxOpcode::Vpmulhw => (LP::_66, OM::_0F, 0xE5),
                AvxOpcode::Vpmulhrsw => (LP::_66, OM::_0F38, 0x0B),
                AvxOpcode::Vpmulhuw => (LP::_66, OM::_0F, 0xE4),
                AvxOpcode::Vpmuldq => (LP::_66, OM::_0F38, 0x28),
                AvxOpcode::Vpmuludq => (LP::_66, OM::_0F, 0xF4),
                AvxOpcode::Vpunpckhwd => (LP::_66, OM::_0F, 0x69),
                AvxOpcode::Vpunpcklwd => (LP::_66, OM::_0F, 0x61),
                AvxOpcode::Vunpcklps => (LP::None, OM::_0F, 0x14),
                AvxOpcode::Vunpckhps => (LP::None, OM::_0F, 0x15),
                AvxOpcode::Vaddps => (LP::None, OM::_0F, 0x58),
                AvxOpcode::Vaddpd => (LP::_66, OM::_0F, 0x58),
                AvxOpcode::Vsubps => (LP::None, OM::_0F, 0x5C),
                AvxOpcode::Vsubpd => (LP::_66, OM::_0F, 0x5C),
                AvxOpcode::Vmulps => (LP::None, OM::_0F, 0x59),
                AvxOpcode::Vmulpd => (LP::_66, OM::_0F, 0x59),
                AvxOpcode::Vdivps => (LP::None, OM::_0F, 0x5E),
                AvxOpcode::Vdivpd => (LP::_66, OM::_0F, 0x5E),
                AvxOpcode::Vpcmpeqb => (LP::_66, OM::_0F, 0x74),
                AvxOpcode::Vpcmpeqw => (LP::_66, OM::_0F, 0x75),
                AvxOpcode::Vpcmpeqd => (LP::_66, OM::_0F, 0x76),
                AvxOpcode::Vpcmpeqq => (LP::_66, OM::_0F38, 0x29),
                AvxOpcode::Vpcmpgtb => (LP::_66, OM::_0F, 0x64),
                AvxOpcode::Vpcmpgtw => (LP::_66, OM::_0F, 0x65),
                AvxOpcode::Vpcmpgtd => (LP::_66, OM::_0F, 0x66),
                AvxOpcode::Vpcmpgtq => (LP::_66, OM::_0F38, 0x37),
                AvxOpcode::Vmovlhps => (LP::None, OM::_0F, 0x16),
                AvxOpcode::Vpminsb => (LP::_66, OM::_0F38, 0x38),
                AvxOpcode::Vpminsw => (LP::_66, OM::_0F, 0xEA),
                AvxOpcode::Vpminsd => (LP::_66, OM::_0F38, 0x39),
                AvxOpcode::Vpmaxsb => (LP::_66, OM::_0F38, 0x3C),
                AvxOpcode::Vpmaxsw => (LP::_66, OM::_0F, 0xEE),
                AvxOpcode::Vpmaxsd => (LP::_66, OM::_0F38, 0x3D),
                AvxOpcode::Vpminub => (LP::_66, OM::_0F, 0xDA),
                AvxOpcode::Vpminuw => (LP::_66, OM::_0F38, 0x3A),
                AvxOpcode::Vpminud => (LP::_66, OM::_0F38, 0x3B),
                AvxOpcode::Vpmaxub => (LP::_66, OM::_0F, 0xDE),
                AvxOpcode::Vpmaxuw => (LP::_66, OM::_0F38, 0x3E),
                AvxOpcode::Vpmaxud => (LP::_66, OM::_0F38, 0x3F),
                AvxOpcode::Vpunpcklbw => (LP::_66, OM::_0F, 0x60),
                AvxOpcode::Vpunpckhbw => (LP::_66, OM::_0F, 0x68),
                AvxOpcode::Vpacksswb => (LP::_66, OM::_0F, 0x63),
                AvxOpcode::Vpackssdw => (LP::_66, OM::_0F, 0x6B),
                AvxOpcode::Vpackuswb => (LP::_66, OM::_0F, 0x67),
                AvxOpcode::Vpackusdw => (LP::_66, OM::_0F38, 0x2B),
                AvxOpcode::Vpmaddwd => (LP::_66, OM::_0F, 0xF5),
                AvxOpcode::Vpmaddubsw => (LP::_66, OM::_0F38, 0x04),
                AvxOpcode::Vpshufb => (LP::_66, OM::_0F38, 0x00),
                AvxOpcode::Vpsllw => (LP::_66, OM::_0F, 0xF1),
                AvxOpcode::Vpslld => (LP::_66, OM::_0F, 0xF2),
                AvxOpcode::Vpsllq => (LP::_66, OM::_0F, 0xF3),
                AvxOpcode::Vpsraw => (LP::_66, OM::_0F, 0xE1),
                AvxOpcode::Vpsrad => (LP::_66, OM::_0F, 0xE2),
                AvxOpcode::Vaddss => (LP::_F3, OM::_0F, 0x58),
                AvxOpcode::Vaddsd => (LP::_F2, OM::_0F, 0x58),
                AvxOpcode::Vmulss => (LP::_F3, OM::_0F, 0x59),
                AvxOpcode::Vmulsd => (LP::_F2, OM::_0F, 0x59),
                AvxOpcode::Vsubss => (LP::_F3, OM::_0F, 0x5C),
                AvxOpcode::Vsubsd => (LP::_F2, OM::_0F, 0x5C),
                AvxOpcode::Vdivss => (LP::_F3, OM::_0F, 0x5E),
                AvxOpcode::Vdivsd => (LP::_F2, OM::_0F, 0x5E),
                AvxOpcode::Vminss => (LP::_F3, OM::_0F, 0x5D),
                AvxOpcode::Vminsd => (LP::_F2, OM::_0F, 0x5D),
                AvxOpcode::Vmaxss => (LP::_F3, OM::_0F, 0x5F),
                AvxOpcode::Vmaxsd => (LP::_F2, OM::_0F, 0x5F),
                AvxOpcode::Vphaddw => (LP::_66, OM::_0F38, 0x01),
                AvxOpcode::Vphaddd => (LP::_66, OM::_0F38, 0x02),
                AvxOpcode::Vpunpckldq => (LP::_66, OM::_0F, 0x62),
                AvxOpcode::Vpunpckhdq => (LP::_66, OM::_0F, 0x6A),
                AvxOpcode::Vpunpcklqdq => (LP::_66, OM::_0F, 0x6C),
                AvxOpcode::Vpunpckhqdq => (LP::_66, OM::_0F, 0x6D),
                AvxOpcode::Vmovsd => (LP::_F2, OM::_0F, 0x10),
                AvxOpcode::Vmovss => (LP::_F3, OM::_0F, 0x10),
                AvxOpcode::Vsqrtss => (LP::_F3, OM::_0F, 0x51),
                AvxOpcode::Vsqrtsd => (LP::_F2, OM::_0F, 0x51),
                AvxOpcode::Vunpcklpd => (LP::_66, OM::_0F, 0x14),
                _ => panic!("unexpected rmir vex opcode {op:?}"),
            };
            VexInstruction::new()
                .length(VexVectorLength::V128)
                .prefix(prefix)
                .map(map)
                .opcode(opcode)
                .reg(dst.to_real_reg().unwrap().hw_enc())
                .vvvv(src1.to_real_reg().unwrap().hw_enc())
                .rm(src2)
                .encode(sink);
        }

        Inst::XmmRmRImmVex {
            op,
            src1,
            src2,
            dst,
            imm,
        } => {
            let dst = dst.to_reg().to_reg();
            let src1 = src1.to_reg();
            let src2 = match src2.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (w, prefix, map, opcode) = match op {
                AvxOpcode::Vcmpps => (false, LegacyPrefixes::None, OpcodeMap::_0F, 0xC2),
                AvxOpcode::Vcmppd => (false, LegacyPrefixes::_66, OpcodeMap::_0F, 0xC2),
                AvxOpcode::Vpalignr => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x0F),
                AvxOpcode::Vinsertps => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x21),
                AvxOpcode::Vshufps => (false, LegacyPrefixes::None, OpcodeMap::_0F, 0xC6),
                AvxOpcode::Vpblendw => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x0E),
                _ => panic!("unexpected rmr_imm_vex opcode {op:?}"),
            };

            VexInstruction::new()
                .length(VexVectorLength::V128)
                .prefix(prefix)
                .map(map)
                .w(w)
                .opcode(opcode)
                .reg(dst.to_real_reg().unwrap().hw_enc())
                .vvvv(src1.to_real_reg().unwrap().hw_enc())
                .rm(src2)
                .imm(*imm)
                .encode(sink);
        }

        Inst::XmmRmRVex3 {
            op,
            src1,
            src2,
            src3,
            dst,
        } => {
            let src1 = src1.to_reg();
            let dst = dst.to_reg().to_reg();
            debug_assert_eq!(src1, dst);
            let src2 = src2.to_reg();
            let src3 = match src3.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (w, map, opcode) = match op {
                AvxOpcode::Vfmadd132ss => (false, OpcodeMap::_0F38, 0x99),
                AvxOpcode::Vfmadd213ss => (false, OpcodeMap::_0F38, 0xA9),
                AvxOpcode::Vfnmadd132ss => (false, OpcodeMap::_0F38, 0x9D),
                AvxOpcode::Vfnmadd213ss => (false, OpcodeMap::_0F38, 0xAD),
                AvxOpcode::Vfmadd132sd => (true, OpcodeMap::_0F38, 0x99),
                AvxOpcode::Vfmadd213sd => (true, OpcodeMap::_0F38, 0xA9),
                AvxOpcode::Vfnmadd132sd => (true, OpcodeMap::_0F38, 0x9D),
                AvxOpcode::Vfnmadd213sd => (true, OpcodeMap::_0F38, 0xAD),
                AvxOpcode::Vfmadd132ps => (false, OpcodeMap::_0F38, 0x98),
                AvxOpcode::Vfmadd213ps => (false, OpcodeMap::_0F38, 0xA8),
                AvxOpcode::Vfnmadd132ps => (false, OpcodeMap::_0F38, 0x9C),
                AvxOpcode::Vfnmadd213ps => (false, OpcodeMap::_0F38, 0xAC),
                AvxOpcode::Vfmadd132pd => (true, OpcodeMap::_0F38, 0x98),
                AvxOpcode::Vfmadd213pd => (true, OpcodeMap::_0F38, 0xA8),
                AvxOpcode::Vfnmadd132pd => (true, OpcodeMap::_0F38, 0x9C),
                AvxOpcode::Vfnmadd213pd => (true, OpcodeMap::_0F38, 0xAC),
                AvxOpcode::Vfmsub132ss => (false, OpcodeMap::_0F38, 0x9B),
                AvxOpcode::Vfmsub213ss => (false, OpcodeMap::_0F38, 0xAB),
                AvxOpcode::Vfnmsub132ss => (false, OpcodeMap::_0F38, 0x9F),
                AvxOpcode::Vfnmsub213ss => (false, OpcodeMap::_0F38, 0xAF),
                AvxOpcode::Vfmsub132sd => (true, OpcodeMap::_0F38, 0x9B),
                AvxOpcode::Vfmsub213sd => (true, OpcodeMap::_0F38, 0xAB),
                AvxOpcode::Vfnmsub132sd => (true, OpcodeMap::_0F38, 0x9F),
                AvxOpcode::Vfnmsub213sd => (true, OpcodeMap::_0F38, 0xAF),
                AvxOpcode::Vfmsub132ps => (false, OpcodeMap::_0F38, 0x9A),
                AvxOpcode::Vfmsub213ps => (false, OpcodeMap::_0F38, 0xAA),
                AvxOpcode::Vfnmsub132ps => (false, OpcodeMap::_0F38, 0x9E),
                AvxOpcode::Vfnmsub213ps => (false, OpcodeMap::_0F38, 0xAE),
                AvxOpcode::Vfmsub132pd => (true, OpcodeMap::_0F38, 0x9A),
                AvxOpcode::Vfmsub213pd => (true, OpcodeMap::_0F38, 0xAA),
                AvxOpcode::Vfnmsub132pd => (true, OpcodeMap::_0F38, 0x9E),
                AvxOpcode::Vfnmsub213pd => (true, OpcodeMap::_0F38, 0xAE),
                _ => unreachable!(),
            };

            VexInstruction::new()
                .length(VexVectorLength::V128)
                .prefix(LegacyPrefixes::_66)
                .map(map)
                .w(w)
                .opcode(opcode)
                .reg(dst.to_real_reg().unwrap().hw_enc())
                .rm(src3)
                .vvvv(src2.to_real_reg().unwrap().hw_enc())
                .encode(sink);
        }

        Inst::XmmUnaryRmRVex { op, src, dst } => {
            let dst = dst.to_reg().to_reg();
            let src = match src.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (prefix, map, opcode) = match op {
                AvxOpcode::Vpmovsxbw => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x20),
                AvxOpcode::Vpmovzxbw => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x30),
                AvxOpcode::Vpmovsxwd => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x23),
                AvxOpcode::Vpmovzxwd => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x33),
                AvxOpcode::Vpmovsxdq => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x25),
                AvxOpcode::Vpmovzxdq => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x35),
                AvxOpcode::Vpabsb => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x1C),
                AvxOpcode::Vpabsw => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x1D),
                AvxOpcode::Vpabsd => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x1E),
                AvxOpcode::Vsqrtps => (LegacyPrefixes::None, OpcodeMap::_0F, 0x51),
                AvxOpcode::Vsqrtpd => (LegacyPrefixes::_66, OpcodeMap::_0F, 0x51),
                AvxOpcode::Vmovdqu => (LegacyPrefixes::_F3, OpcodeMap::_0F, 0x6F),
                AvxOpcode::Vmovups => (LegacyPrefixes::None, OpcodeMap::_0F, 0x10),
                AvxOpcode::Vmovupd => (LegacyPrefixes::_66, OpcodeMap::_0F, 0x10),

                // Note that for `vmov{s,d}` the `inst.isle` rules should
                // statically ensure that only `Amode` operands are used here.
                // Otherwise the other encodings of `vmovss` are more like
                // 2-operand instructions which this unary encoding does not
                // have.
                AvxOpcode::Vmovss => match &src {
                    RegisterOrAmode::Amode(_) => (LegacyPrefixes::_F3, OpcodeMap::_0F, 0x10),
                    _ => unreachable!(),
                },
                AvxOpcode::Vmovsd => match &src {
                    RegisterOrAmode::Amode(_) => (LegacyPrefixes::_F2, OpcodeMap::_0F, 0x10),
                    _ => unreachable!(),
                },

                AvxOpcode::Vpbroadcastb => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x78),
                AvxOpcode::Vpbroadcastw => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x79),
                AvxOpcode::Vpbroadcastd => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x58),
                AvxOpcode::Vbroadcastss => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x18),

                _ => panic!("unexpected rmr_imm_vex opcode {op:?}"),
            };

            VexInstruction::new()
                .length(VexVectorLength::V128)
                .prefix(prefix)
                .map(map)
                .opcode(opcode)
                .reg(dst.to_real_reg().unwrap().hw_enc())
                .rm(src)
                .encode(sink);
        }

        Inst::XmmMovRMVex { op, src, dst } => {
            let src = src.to_reg();
            let dst = dst.clone().finalize(state.frame_layout(), sink);

            let (prefix, map, opcode) = match op {
                AvxOpcode::Vmovdqu => (LegacyPrefixes::_F3, OpcodeMap::_0F, 0x7F),
                AvxOpcode::Vmovss => (LegacyPrefixes::_F3, OpcodeMap::_0F, 0x11),
                AvxOpcode::Vmovsd => (LegacyPrefixes::_F2, OpcodeMap::_0F, 0x11),
                AvxOpcode::Vmovups => (LegacyPrefixes::None, OpcodeMap::_0F, 0x11),
                AvxOpcode::Vmovupd => (LegacyPrefixes::_66, OpcodeMap::_0F, 0x11),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            VexInstruction::new()
                .length(VexVectorLength::V128)
                .prefix(prefix)
                .map(map)
                .opcode(opcode)
                .rm(dst)
                .reg(src.to_real_reg().unwrap().hw_enc())
                .encode(sink);
        }

        Inst::XmmMovRMImmVex { op, src, dst, imm } => {
            let src = src.to_reg();
            let dst = dst.clone().finalize(state.frame_layout(), sink);

            let (w, prefix, map, opcode) = match op {
                AvxOpcode::Vpextrb => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x14),
                AvxOpcode::Vpextrw => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x15),
                AvxOpcode::Vpextrd => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x16),
                AvxOpcode::Vpextrq => (true, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x16),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            VexInstruction::new()
                .length(VexVectorLength::V128)
                .w(w)
                .prefix(prefix)
                .map(map)
                .opcode(opcode)
                .rm(dst)
                .reg(src.to_real_reg().unwrap().hw_enc())
                .imm(*imm)
                .encode(sink);
        }

        Inst::XmmToGprImmVex { op, src, dst, imm } => {
            let src = src.to_reg();
            let dst = dst.to_reg().to_reg();

            let (w, prefix, map, opcode) = match op {
                AvxOpcode::Vpextrb => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x14),
                AvxOpcode::Vpextrw => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x15),
                AvxOpcode::Vpextrd => (false, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x16),
                AvxOpcode::Vpextrq => (true, LegacyPrefixes::_66, OpcodeMap::_0F3A, 0x16),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            VexInstruction::new()
                .length(VexVectorLength::V128)
                .w(w)
                .prefix(prefix)
                .map(map)
                .opcode(opcode)
                .rm(dst.to_real_reg().unwrap().hw_enc())
                .reg(src.to_real_reg().unwrap().hw_enc())
                .imm(*imm)
                .encode(sink);
        }

        Inst::XmmCmpRmRVex { op, src1, src2 } => {
            let src1 = src1.to_reg();
            let src2 = match src2.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };

            let (prefix, map, opcode) = match op {
                AvxOpcode::Vucomiss => (LegacyPrefixes::None, OpcodeMap::_0F, 0x2E),
                AvxOpcode::Vucomisd => (LegacyPrefixes::_66, OpcodeMap::_0F, 0x2E),
                AvxOpcode::Vptest => (LegacyPrefixes::_66, OpcodeMap::_0F38, 0x17),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };

            VexInstruction::new()
                .length(VexVectorLength::V128)
                .prefix(prefix)
                .map(map)
                .opcode(opcode)
                .rm(src2)
                .reg(src1.to_real_reg().unwrap().hw_enc())
                .encode(sink);
        }

        Inst::XmmRmREvex {
            op,
            src1,
            src2,
            dst,
        }
        | Inst::XmmRmREvex3 {
            op,
            src1: _, // `dst` reuses `src1`.
            src2: src1,
            src3: src2,
            dst,
        } => {
            let reused_src = match inst {
                Inst::XmmRmREvex3 { src1, .. } => Some(src1.to_reg()),
                _ => None,
            };
            let src1 = src1.to_reg();
            let src2 = match src2.clone().to_reg_mem().clone() {
                RegMem::Reg { reg } => {
                    RegisterOrAmode::Register(reg.to_real_reg().unwrap().hw_enc().into())
                }
                RegMem::Mem { addr } => {
                    RegisterOrAmode::Amode(addr.finalize(state.frame_layout(), sink))
                }
            };
            let dst = dst.to_reg().to_reg();
            if let Some(src1) = reused_src {
                debug_assert_eq!(src1, dst);
            }

            let (w, opcode, map) = match op {
                Avx512Opcode::Vpermi2b => (false, 0x75, OpcodeMap::_0F38),
                Avx512Opcode::Vpmullq => (true, 0x40, OpcodeMap::_0F38),
                Avx512Opcode::Vpsraq => (true, 0xE2, OpcodeMap::_0F),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            EvexInstruction::new()
                .length(EvexVectorLength::V128)
                .prefix(LegacyPrefixes::_66)
                .map(map)
                .w(w)
                .opcode(opcode)
                .tuple_type(op.tuple_type())
                .reg(dst.to_real_reg().unwrap().hw_enc())
                .vvvvv(src1.to_real_reg().unwrap().hw_enc())
                .rm(src2)
                .encode(sink);
        }

        Inst::XmmMinMaxSeq {
            size,
            is_min,
            lhs,
            rhs,
            dst,
        } => {
            let rhs = rhs.to_reg();
            let lhs = lhs.to_reg();
            let dst = dst.to_writable_reg();
            debug_assert_eq!(rhs, dst.to_reg());

            // Generates the following sequence:
            // cmpss/cmpsd %lhs, %rhs_dst
            // jnz do_min_max
            // jp propagate_nan
            //
            // ;; ordered and equal: propagate the sign bit (for -0 vs 0):
            // {and,or}{ss,sd} %lhs, %rhs_dst
            // j done
            //
            // ;; to get the desired NaN behavior (signalling NaN transformed into a quiet NaN, the
            // ;; NaN value is returned), we add both inputs.
            // propagate_nan:
            // add{ss,sd} %lhs, %rhs_dst
            // j done
            //
            // do_min_max:
            // {min,max}{ss,sd} %lhs, %rhs_dst
            //
            // done:
            let done = sink.get_label();
            let propagate_nan = sink.get_label();
            let do_min_max = sink.get_label();

            let (add_op, cmp_op, and_op, or_op, min_max_op) = match size {
                OperandSize::Size32 => (
                    asm::inst::addss_a::new(dst, lhs).into(),
                    SseOpcode::Ucomiss,
                    asm::inst::andps_a::new(dst, lhs).into(),
                    asm::inst::orps_a::new(dst, lhs).into(),
                    if *is_min {
                        asm::inst::minss_a::new(dst, lhs).into()
                    } else {
                        asm::inst::maxss_a::new(dst, lhs).into()
                    },
                ),
                OperandSize::Size64 => (
                    asm::inst::addsd_a::new(dst, lhs).into(),
                    SseOpcode::Ucomisd,
                    asm::inst::andpd_a::new(dst, lhs).into(),
                    asm::inst::orpd_a::new(dst, lhs).into(),
                    if *is_min {
                        asm::inst::minsd_a::new(dst, lhs).into()
                    } else {
                        asm::inst::maxsd_a::new(dst, lhs).into()
                    },
                ),
                _ => unreachable!(),
            };

            let inst = Inst::xmm_cmp_rm_r(cmp_op, dst.to_reg(), RegMem::reg(lhs));
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NZ, do_min_max);
            one_way_jmp(sink, CC::P, propagate_nan);

            // Ordered and equal. The operands are bit-identical unless they are zero
            // and negative zero. These instructions merge the sign bits in that
            // case, and are no-ops otherwise.
            let inst = if *is_min { or_op } else { and_op };
            Inst::External { inst }.emit(sink, info, state);

            let inst = Inst::jmp_known(done);
            inst.emit(sink, info, state);

            // x86's min/max are not symmetric; if either operand is a NaN, they return the
            // read-only operand: perform an addition between the two operands, which has the
            // desired NaN propagation effects.
            sink.bind_label(propagate_nan, state.ctrl_plane_mut());
            Inst::External { inst: add_op }.emit(sink, info, state);

            one_way_jmp(sink, CC::P, done);

            sink.bind_label(do_min_max, state.ctrl_plane_mut());
            Inst::External { inst: min_max_op }.emit(sink, info, state);

            sink.bind_label(done, state.ctrl_plane_mut());
        }

        Inst::XmmRmRImm {
            op,
            src1,
            src2,
            dst,
            imm,
            size,
        } => {
            let src1 = *src1;
            let dst = dst.to_reg();
            let src2 = src2.clone();
            debug_assert_eq!(src1, dst);

            let (prefix, opcode, len) = match op {
                SseOpcode::Cmpps => (LegacyPrefixes::None, 0x0FC2, 2),
                SseOpcode::Cmppd => (LegacyPrefixes::_66, 0x0FC2, 2),
                SseOpcode::Cmpss => (LegacyPrefixes::_F3, 0x0FC2, 2),
                SseOpcode::Cmpsd => (LegacyPrefixes::_F2, 0x0FC2, 2),
                SseOpcode::Insertps => (LegacyPrefixes::_66, 0x0F3A21, 3),
                SseOpcode::Palignr => (LegacyPrefixes::_66, 0x0F3A0F, 3),
                SseOpcode::Shufps => (LegacyPrefixes::None, 0x0FC6, 2),
                SseOpcode::Pblendw => (LegacyPrefixes::_66, 0x0F3A0E, 3),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            let rex = RexFlags::from(*size);
            match src2 {
                RegMem::Reg { reg } => {
                    emit_std_reg_reg(sink, prefix, opcode, len, dst, reg, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink);
                    // N.B.: bytes_at_end == 1, because of the `imm` byte below.
                    emit_std_reg_mem(sink, prefix, opcode, len, dst, addr, rex, 1);
                }
            }
            sink.put1(*imm);
        }

        Inst::XmmUninitializedValue { .. } | Inst::GprUninitializedValue { .. } => {
            // These instruction formats only exist to declare a register as a
            // `def`; no code is emitted. This is always immediately followed by
            // an instruction, such as `xor <tmp>, <tmp>`, that semantically
            // reads this undefined value but arithmetically produces the same
            // result regardless of its value.
        }

        Inst::XmmCmpRmR { op, src1, src2 } => {
            let src1 = src1.to_reg();
            let src2 = src2.clone().to_reg_mem().clone();

            let rex = RexFlags::clear_w();
            let (prefix, opcode, len) = match op {
                SseOpcode::Ptest => (LegacyPrefixes::_66, 0x0F3817, 3),
                SseOpcode::Ucomisd => (LegacyPrefixes::_66, 0x0F2E, 2),
                SseOpcode::Ucomiss => (LegacyPrefixes::None, 0x0F2E, 2),
                _ => unimplemented!("Emit xmm cmp rm r"),
            };

            match src2 {
                RegMem::Reg { reg } => {
                    emit_std_reg_reg(sink, prefix, opcode, len, src1, reg, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state.frame_layout(), sink);
                    emit_std_reg_mem(sink, prefix, opcode, len, src1, addr, rex, 0);
                }
            }
        }

        Inst::CvtUint64ToFloatSeq {
            dst_size,
            src,
            dst,
            tmp_gpr1,
            tmp_gpr2,
        } => {
            let src = src.to_reg();
            let dst = dst.to_writable_reg();
            let tmp_gpr1 = tmp_gpr1.to_writable_reg();
            let tmp_gpr2 = tmp_gpr2.to_writable_reg();

            // Note: this sequence is specific to 64-bit mode; a 32-bit mode would require a
            // different sequence.
            //
            // Emit the following sequence:
            //
            //  cmp 0, %src
            //  jl handle_negative
            //
            //  ;; handle positive, which can't overflow
            //  cvtsi2sd/cvtsi2ss %src, %dst
            //  j done
            //
            //  ;; handle negative: see below for an explanation of what it's doing.
            //  handle_negative:
            //  mov %src, %tmp_gpr1
            //  shr $1, %tmp_gpr1
            //  mov %src, %tmp_gpr2
            //  and $1, %tmp_gpr2
            //  or %tmp_gpr1, %tmp_gpr2
            //  cvtsi2sd/cvtsi2ss %tmp_gpr2, %dst
            //  addsd/addss %dst, %dst
            //
            //  done:

            assert_ne!(src, tmp_gpr1.to_reg());
            assert_ne!(src, tmp_gpr2.to_reg());

            let handle_negative = sink.get_label();
            let done = sink.get_label();

            // If x seen as a signed int64 is not negative, a signed-conversion will do the right
            // thing.
            // TODO use tst src, src here.
            let inst = Inst::cmp_rmi_r(OperandSize::Size64, src, RegMemImm::imm(0));
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::L, handle_negative);

            // Handle a positive int64, which is the "easy" case: a signed conversion will do the
            // right thing.
            emit_signed_cvt(
                sink,
                info,
                state,
                src,
                dst,
                *dst_size == OperandSize::Size64,
            );

            let inst = Inst::jmp_known(done);
            inst.emit(sink, info, state);

            sink.bind_label(handle_negative, state.ctrl_plane_mut());

            // Divide x by two to get it in range for the signed conversion, keep the LSB, and
            // scale it back up on the FP side.
            let inst = Inst::gen_move(tmp_gpr1, src, types::I64);
            inst.emit(sink, info, state);

            // tmp_gpr1 := src >> 1
            Inst::External {
                inst: asm::inst::shrq_mi::new(tmp_gpr1, 1).into(),
            }
            .emit(sink, info, state);

            let inst = Inst::gen_move(tmp_gpr2, src, types::I64);
            inst.emit(sink, info, state);

            Inst::External {
                inst: asm::inst::andq_mi_sxb::new(tmp_gpr2, 1).into(),
            }
            .emit(sink, info, state);

            Inst::External {
                inst: asm::inst::orq_rm::new(tmp_gpr2, tmp_gpr1).into(),
            }
            .emit(sink, info, state);

            emit_signed_cvt(
                sink,
                info,
                state,
                tmp_gpr2.to_reg(),
                dst,
                *dst_size == OperandSize::Size64,
            );

            let inst = match *dst_size {
                OperandSize::Size64 => asm::inst::addsd_a::new(dst, dst.to_reg()).into(),
                OperandSize::Size32 => asm::inst::addss_a::new(dst, dst.to_reg()).into(),
                _ => unreachable!(),
            };
            Inst::External { inst }.emit(sink, info, state);

            sink.bind_label(done, state.ctrl_plane_mut());
        }

        Inst::CvtFloatToSintSeq {
            src_size,
            dst_size,
            is_saturating,
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
        } => {
            use OperandSize::*;

            let src = src.to_reg();
            let dst = dst.to_writable_reg();
            let tmp_gpr = tmp_gpr.to_writable_reg();
            let tmp_xmm = tmp_xmm.to_writable_reg();

            // Emits the following common sequence:
            //
            // cvttss2si/cvttsd2si %src, %dst
            // cmp %dst, 1
            // jno done
            //
            // Then, for saturating conversions:
            //
            // ;; check for NaN
            // cmpss/cmpsd %src, %src
            // jnp not_nan
            // xor %dst, %dst
            //
            // ;; positive inputs get saturated to INT_MAX; negative ones to INT_MIN, which is
            // ;; already in %dst.
            // xorpd %tmp_xmm, %tmp_xmm
            // cmpss/cmpsd %src, %tmp_xmm
            // jnb done
            // mov/movaps $INT_MAX, %dst
            //
            // done:
            //
            // Then, for non-saturating conversions:
            //
            // ;; check for NaN
            // cmpss/cmpsd %src, %src
            // jnp not_nan
            // ud2 trap BadConversionToInteger
            //
            // ;; check if INT_MIN was the correct result, against a magic constant:
            // not_nan:
            // movaps/mov $magic, %tmp_gpr
            // movq/movd %tmp_gpr, %tmp_xmm
            // cmpss/cmpsd %tmp_xmm, %src
            // jnb/jnbe $check_positive
            // ud2 trap IntegerOverflow
            //
            // ;; if positive, it was a real overflow
            // check_positive:
            // xorpd %tmp_xmm, %tmp_xmm
            // cmpss/cmpsd %src, %tmp_xmm
            // jnb done
            // ud2 trap IntegerOverflow
            //
            // done:

            let cmp_op = match src_size {
                Size64 => SseOpcode::Ucomisd,
                Size32 => SseOpcode::Ucomiss,
                _ => unreachable!(),
            };

            let cvtt_op = |dst, src| Inst::External {
                inst: match (*src_size, *dst_size) {
                    (Size32, Size32) => asm::inst::cvttss2si_a::new(dst, src).into(),
                    (Size32, Size64) => asm::inst::cvttss2si_aq::new(dst, src).into(),
                    (Size64, Size32) => asm::inst::cvttsd2si_a::new(dst, src).into(),
                    (Size64, Size64) => asm::inst::cvttsd2si_aq::new(dst, src).into(),
                    _ => unreachable!(),
                },
            };

            let done = sink.get_label();

            // The truncation.
            cvtt_op(dst, src).emit(sink, info, state);

            // Compare against 1, in case of overflow the dst operand was INT_MIN.
            let inst = Inst::cmp_rmi_r(*dst_size, dst.to_reg(), RegMemImm::imm(1));
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NO, done); // no overflow => done

            // Check for NaN.

            let inst = Inst::xmm_cmp_rm_r(cmp_op, src, RegMem::reg(src));
            inst.emit(sink, info, state);

            if *is_saturating {
                let not_nan = sink.get_label();
                one_way_jmp(sink, CC::NP, not_nan); // go to not_nan if not a NaN

                // For NaN, emit 0.
                let inst = match *dst_size {
                    OperandSize::Size32 => asm::inst::xorl_rm::new(dst, dst).into(),
                    OperandSize::Size64 => asm::inst::xorq_rm::new(dst, dst).into(),
                    _ => unreachable!(),
                };
                Inst::External { inst }.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);

                sink.bind_label(not_nan, state.ctrl_plane_mut());

                // If the input was positive, saturate to INT_MAX.

                // Zero out tmp_xmm.
                let inst = asm::inst::xorpd_a::new(tmp_xmm, tmp_xmm.to_reg()).into();
                Inst::External { inst }.emit(sink, info, state);

                let inst = Inst::xmm_cmp_rm_r(cmp_op, tmp_xmm.to_reg(), RegMem::reg(src));
                inst.emit(sink, info, state);

                // Jump if >= to done.
                one_way_jmp(sink, CC::NB, done);

                // Otherwise, put INT_MAX.
                if *dst_size == OperandSize::Size64 {
                    let inst = Inst::imm(OperandSize::Size64, 0x7fffffffffffffff, dst);
                    inst.emit(sink, info, state);
                } else {
                    let inst = Inst::imm(OperandSize::Size32, 0x7fffffff, dst);
                    inst.emit(sink, info, state);
                }
            } else {
                let inst = Inst::trap_if(CC::P, TrapCode::BAD_CONVERSION_TO_INTEGER);
                inst.emit(sink, info, state);

                // Check if INT_MIN was the correct result: determine the smallest floating point
                // number that would convert to INT_MIN, put it in a temporary register, and compare
                // against the src register.
                // If the src register is less (or in some cases, less-or-equal) than the threshold,
                // trap!

                let mut no_overflow_cc = CC::NB; // >=
                let output_bits = dst_size.to_bits();
                match *src_size {
                    OperandSize::Size32 => {
                        let cst = (-Ieee32::pow2(output_bits - 1)).bits();
                        let inst = Inst::imm(OperandSize::Size32, cst as u64, tmp_gpr);
                        inst.emit(sink, info, state);
                    }
                    OperandSize::Size64 => {
                        // An f64 can represent `i32::min_value() - 1` exactly with precision to spare,
                        // so there are values less than -2^(N-1) that convert correctly to INT_MIN.
                        let cst = if output_bits < 64 {
                            no_overflow_cc = CC::NBE; // >
                            Ieee64::fcvt_to_sint_negative_overflow(output_bits)
                        } else {
                            -Ieee64::pow2(output_bits - 1)
                        };
                        let inst = Inst::imm(OperandSize::Size64, cst.bits(), tmp_gpr);
                        inst.emit(sink, info, state);
                    }
                    _ => unreachable!(),
                }

                let inst = {
                    let tmp_xmm: WritableXmm = tmp_xmm.map(|r| Xmm::new(r).unwrap());
                    match src_size {
                        Size32 => asm::inst::movd_a::new(tmp_xmm, tmp_gpr).into(),
                        Size64 => asm::inst::movq_a::new(tmp_xmm, tmp_gpr).into(),
                        _ => unreachable!(),
                    }
                };
                Inst::External { inst }.emit(sink, info, state);

                let inst = Inst::xmm_cmp_rm_r(cmp_op, src, RegMem::reg(tmp_xmm.to_reg()));
                inst.emit(sink, info, state);

                // no trap if src >= or > threshold
                let inst = Inst::trap_if(no_overflow_cc.invert(), TrapCode::INTEGER_OVERFLOW);
                inst.emit(sink, info, state);

                // If positive, it was a real overflow.

                // Zero out the tmp_xmm register.
                let inst = asm::inst::xorpd_a::new(tmp_xmm, tmp_xmm.to_reg()).into();
                Inst::External { inst }.emit(sink, info, state);

                let inst = Inst::xmm_cmp_rm_r(cmp_op, tmp_xmm.to_reg(), RegMem::reg(src));
                inst.emit(sink, info, state);

                // no trap if 0 >= src
                let inst = Inst::trap_if(CC::B, TrapCode::INTEGER_OVERFLOW);
                inst.emit(sink, info, state);
            }

            sink.bind_label(done, state.ctrl_plane_mut());
        }

        Inst::CvtFloatToUintSeq {
            src_size,
            dst_size,
            is_saturating,
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
            tmp_xmm2,
        } => {
            use OperandSize::*;

            let src = src.to_reg();
            let dst = dst.to_writable_reg();
            let tmp_gpr = tmp_gpr.to_writable_reg();
            let tmp_xmm = tmp_xmm.to_writable_reg();
            let tmp_xmm2 = tmp_xmm2.to_writable_reg();

            // The only difference in behavior between saturating and non-saturating is how we
            // handle errors. Emits the following sequence:
            //
            // movaps/mov 2**(int_width - 1), %tmp_gpr
            // movq/movd %tmp_gpr, %tmp_xmm
            // cmpss/cmpsd %tmp_xmm, %src
            // jnb is_large
            //
            // ;; check for NaN inputs
            // jnp not_nan
            // -- non-saturating: ud2 trap BadConversionToInteger
            // -- saturating: xor %dst, %dst; j done
            //
            // not_nan:
            // cvttss2si/cvttsd2si %src, %dst
            // cmp 0, %dst
            // jnl done
            // -- non-saturating: ud2 trap IntegerOverflow
            // -- saturating: xor %dst, %dst; j done
            //
            // is_large:
            // mov %src, %tmp_xmm2
            // subss/subsd %tmp_xmm, %tmp_xmm2
            // cvttss2si/cvttss2sd %tmp_x, %dst
            // cmp 0, %dst
            // jnl next_is_large
            // -- non-saturating: ud2 trap IntegerOverflow
            // -- saturating: movaps $UINT_MAX, %dst; j done
            //
            // next_is_large:
            // add 2**(int_width -1), %dst ;; 2 instructions for 64-bits integers
            //
            // done:

            assert_ne!(tmp_xmm.to_reg(), src, "tmp_xmm clobbers src!");

            let cmp_op = match src_size {
                Size32 => SseOpcode::Ucomiss,
                Size64 => SseOpcode::Ucomisd,
                _ => unreachable!(),
            };

            let xor_op = |dst, src| Inst::External {
                inst: match *dst_size {
                    Size32 => asm::inst::xorl_rm::new(dst, src).into(),
                    Size64 => asm::inst::xorq_rm::new(dst, src).into(),
                    _ => unreachable!(),
                },
            };

            let subs_op = |dst, src| Inst::External {
                inst: match *src_size {
                    Size32 => asm::inst::subss_a::new(dst, src).into(),
                    Size64 => asm::inst::subsd_a::new(dst, src).into(),
                    _ => unreachable!(),
                },
            };

            let cvtt_op = |dst, src| Inst::External {
                inst: match (*src_size, *dst_size) {
                    (Size32, Size32) => asm::inst::cvttss2si_a::new(dst, src).into(),
                    (Size32, Size64) => asm::inst::cvttss2si_aq::new(dst, src).into(),
                    (Size64, Size32) => asm::inst::cvttsd2si_a::new(dst, src).into(),
                    (Size64, Size64) => asm::inst::cvttsd2si_aq::new(dst, src).into(),
                    _ => unreachable!(),
                },
            };

            let done = sink.get_label();

            let cst = match src_size {
                OperandSize::Size32 => Ieee32::pow2(dst_size.to_bits() - 1).bits() as u64,
                OperandSize::Size64 => Ieee64::pow2(dst_size.to_bits() - 1).bits(),
                _ => unreachable!(),
            };

            let inst = Inst::imm(*src_size, cst, tmp_gpr);
            inst.emit(sink, info, state);

            let inst = {
                let tmp_xmm: WritableXmm = tmp_xmm.map(|r| Xmm::new(r).unwrap());
                match src_size {
                    Size32 => asm::inst::movd_a::new(tmp_xmm, tmp_gpr).into(),
                    Size64 => asm::inst::movq_a::new(tmp_xmm, tmp_gpr).into(),
                    _ => unreachable!(),
                }
            };
            Inst::External { inst }.emit(sink, info, state);

            let inst = Inst::xmm_cmp_rm_r(cmp_op, src, RegMem::reg(tmp_xmm.to_reg()));
            inst.emit(sink, info, state);

            let handle_large = sink.get_label();
            one_way_jmp(sink, CC::NB, handle_large); // jump to handle_large if src >= large_threshold

            if *is_saturating {
                // If not NaN jump over this 0-return, otherwise return 0
                let not_nan = sink.get_label();
                one_way_jmp(sink, CC::NP, not_nan);

                xor_op(dst, dst).emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);
                sink.bind_label(not_nan, state.ctrl_plane_mut());
            } else {
                // Trap.
                let inst = Inst::trap_if(CC::P, TrapCode::BAD_CONVERSION_TO_INTEGER);
                inst.emit(sink, info, state);
            }

            // Actual truncation for small inputs: if the result is not positive, then we had an
            // overflow.

            cvtt_op(dst, src).emit(sink, info, state);

            let inst = Inst::cmp_rmi_r(*dst_size, dst.to_reg(), RegMemImm::imm(0));
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NL, done); // if dst >= 0, jump to done

            if *is_saturating {
                // The input was "small" (< 2**(width -1)), so the only way to get an integer
                // overflow is because the input was too small: saturate to the min value, i.e. 0.
                let inst = match *dst_size {
                    OperandSize::Size32 => asm::inst::xorl_rm::new(dst, dst).into(),
                    OperandSize::Size64 => asm::inst::xorq_rm::new(dst, dst).into(),
                    _ => unreachable!(),
                };
                Inst::External { inst }.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);
            } else {
                // Trap.
                let inst = Inst::trap(TrapCode::INTEGER_OVERFLOW);
                inst.emit(sink, info, state);
            }

            // Now handle large inputs.

            sink.bind_label(handle_large, state.ctrl_plane_mut());

            let inst = Inst::gen_move(tmp_xmm2, src, types::F64);
            inst.emit(sink, info, state);

            subs_op(tmp_xmm2, tmp_xmm.to_reg()).emit(sink, info, state);

            cvtt_op(dst, tmp_xmm2.to_reg()).emit(sink, info, state);

            let inst = Inst::cmp_rmi_r(*dst_size, dst.to_reg(), RegMemImm::imm(0));
            inst.emit(sink, info, state);

            if *is_saturating {
                let next_is_large = sink.get_label();
                one_way_jmp(sink, CC::NL, next_is_large); // if dst >= 0, jump to next_is_large

                // The input was "large" (>= 2**(width -1)), so the only way to get an integer
                // overflow is because the input was too large: saturate to the max value.
                let inst = Inst::imm(
                    OperandSize::Size64,
                    if *dst_size == OperandSize::Size64 {
                        u64::max_value()
                    } else {
                        u32::max_value() as u64
                    },
                    dst,
                );
                inst.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);
                sink.bind_label(next_is_large, state.ctrl_plane_mut());
            } else {
                let inst = Inst::trap_if(CC::L, TrapCode::INTEGER_OVERFLOW);
                inst.emit(sink, info, state);
            }

            if *dst_size == OperandSize::Size64 {
                let inst = Inst::imm(OperandSize::Size64, 1 << 63, tmp_gpr);
                inst.emit(sink, info, state);

                let inst = Inst::External {
                    inst: asm::inst::addq_rm::new(dst, tmp_gpr).into(),
                };
                inst.emit(sink, info, state);
            } else {
                let inst = Inst::External {
                    inst: asm::inst::addl_mi::new(dst, asm::Imm32::new(1 << 31)).into(),
                };
                inst.emit(sink, info, state);
            }

            sink.bind_label(done, state.ctrl_plane_mut());
        }

        Inst::LoadExtName {
            dst,
            name,
            offset,
            distance,
        } => {
            let dst = dst.to_reg();

            if info.flags.is_pic() {
                // Generates: movq symbol@GOTPCREL(%rip), %dst
                let enc_dst = int_reg_enc(dst);
                sink.put1(0x48 | ((enc_dst >> 3) & 1) << 2);
                sink.put1(0x8B);
                sink.put1(0x05 | ((enc_dst & 7) << 3));
                emit_reloc(sink, Reloc::X86GOTPCRel4, name, -4);
                sink.put4(0);
                // Offset in the relocation above applies to the address of the *GOT entry*, not
                // the loaded address; so we emit a separate add or sub instruction if needed.
                if *offset < 0 {
                    assert!(*offset >= -i32::MAX as i64);
                    sink.put1(0x48 | ((enc_dst >> 3) & 1));
                    sink.put1(0x81);
                    sink.put1(0xe8 | (enc_dst & 7));
                    sink.put4((-*offset) as u32);
                } else if *offset > 0 {
                    assert!(*offset <= i32::MAX as i64);
                    sink.put1(0x48 | ((enc_dst >> 3) & 1));
                    sink.put1(0x81);
                    sink.put1(0xc0 | (enc_dst & 7));
                    sink.put4(*offset as u32);
                }
            } else if distance == &RelocDistance::Near {
                // If we know the distance to the name is within 2GB (e.g., a module-local function),
                // we can generate a RIP-relative address, with a relocation.
                // Generates: lea $name(%rip), $dst
                let enc_dst = int_reg_enc(dst);
                sink.put1(0x48 | ((enc_dst >> 3) & 1) << 2);
                sink.put1(0x8D);
                sink.put1(0x05 | ((enc_dst & 7) << 3));
                emit_reloc(sink, Reloc::X86CallPCRel4, name, -4);
                sink.put4(0);
            } else {
                // The full address can be encoded in the register, with a relocation.
                // Generates: movabsq $name, %dst
                let enc_dst = int_reg_enc(dst);
                sink.put1(0x48 | ((enc_dst >> 3) & 1));
                sink.put1(0xB8 | (enc_dst & 7));
                emit_reloc(sink, Reloc::Abs8, name, *offset);
                sink.put8(0);
            }
        }

        Inst::LockCmpxchg {
            ty,
            replacement,
            expected,
            mem,
            dst_old,
        } => {
            let replacement = *replacement;
            let expected = *expected;
            let dst_old = dst_old.to_reg();
            let mem = mem.clone();

            debug_assert_eq!(expected, regs::rax());
            debug_assert_eq!(dst_old, regs::rax());

            // lock cmpxchg{b,w,l,q} %replacement, (mem)
            // Note that 0xF0 is the Lock prefix.
            let (prefix, opcodes) = match *ty {
                types::I8 => (LegacyPrefixes::_F0, 0x0FB0),
                types::I16 => (LegacyPrefixes::_66F0, 0x0FB1),
                types::I32 => (LegacyPrefixes::_F0, 0x0FB1),
                types::I64 => (LegacyPrefixes::_F0, 0x0FB1),
                _ => unreachable!(),
            };
            let rex = RexFlags::from((OperandSize::from_ty(*ty), replacement));
            let amode = mem.finalize(state.frame_layout(), sink);
            emit_std_reg_mem(sink, prefix, opcodes, 2, replacement, &amode, rex, 0);
        }

        Inst::LockCmpxchg16b {
            replacement_low,
            replacement_high,
            expected_low,
            expected_high,
            mem,
            dst_old_low,
            dst_old_high,
        } => {
            let mem = mem.clone();
            debug_assert_eq!(*replacement_low, regs::rbx());
            debug_assert_eq!(*replacement_high, regs::rcx());
            debug_assert_eq!(*expected_low, regs::rax());
            debug_assert_eq!(*expected_high, regs::rdx());
            debug_assert_eq!(dst_old_low.to_reg(), regs::rax());
            debug_assert_eq!(dst_old_high.to_reg(), regs::rdx());

            let amode = mem.finalize(state.frame_layout(), sink);
            // lock cmpxchg16b (mem)
            // Note that 0xF0 is the Lock prefix.
            emit_std_enc_mem(
                sink,
                LegacyPrefixes::_F0,
                0x0FC7,
                2,
                1,
                &amode,
                RexFlags::set_w(),
                0,
            );
        }

        Inst::LockXadd {
            size,
            operand,
            mem,
            dst_old,
        } => {
            debug_assert_eq!(dst_old.to_reg(), *operand);
            // lock xadd{b,w,l,q} %operand, (mem)
            // Note that 0xF0 is the Lock prefix.
            let (prefix, opcodes) = match size {
                OperandSize::Size8 => (LegacyPrefixes::_F0, 0x0FC0),
                OperandSize::Size16 => (LegacyPrefixes::_66F0, 0x0FC1),
                OperandSize::Size32 => (LegacyPrefixes::_F0, 0x0FC1),
                OperandSize::Size64 => (LegacyPrefixes::_F0, 0x0FC1),
            };
            let rex = RexFlags::from((*size, *operand));
            let amode = mem.finalize(state.frame_layout(), sink);
            emit_std_reg_mem(sink, prefix, opcodes, 2, *operand, &amode, rex, 0);
        }

        Inst::Xchg {
            size,
            operand,
            mem,
            dst_old,
        } => {
            debug_assert_eq!(dst_old.to_reg(), *operand);
            // xchg{b,w,l,q} %operand, (mem)
            let (prefix, opcodes) = match size {
                OperandSize::Size8 => (LegacyPrefixes::None, 0x86),
                OperandSize::Size16 => (LegacyPrefixes::_66, 0x87),
                OperandSize::Size32 => (LegacyPrefixes::None, 0x87),
                OperandSize::Size64 => (LegacyPrefixes::None, 0x87),
            };
            let rex = RexFlags::from((*size, *operand));
            let amode = mem.finalize(state.frame_layout(), sink);
            emit_std_reg_mem(sink, prefix, opcodes, 1, *operand, &amode, rex, 0);
        }

        Inst::AtomicRmwSeq {
            ty,
            op,
            mem,
            operand,
            temp,
            dst_old,
        } => {
            let operand = *operand;
            let temp = *temp;
            let dst_old = *dst_old;
            debug_assert_eq!(dst_old.to_reg(), regs::rax());
            let mem = mem.finalize(state.frame_layout(), sink).clone();

            // Emit this:
            //    mov{zbq,zwq,zlq,q}     (%r_address), %rax    // rax = old value
            //  again:
            //    movq                   %rax, %r_temp         // rax = old value, r_temp = old value
            //    `op`q                  %r_operand, %r_temp   // rax = old value, r_temp = new value
            //    lock cmpxchg{b,w,l,q}  %r_temp, (%r_address) // try to store new value
            //    jnz again // If this is taken, rax will have a "revised" old value
            //
            // Operand conventions: IN:  %r_address, %r_operand OUT: %rax (old
            //    value), %r_temp (trashed), %rflags (trashed)
            let again_label = sink.get_label();

            // mov{zbq,zwq,zlq,q} (%r_address), %rax
            // No need to call `add_trap` here, since the `i1` emit will do that.
            let i1 = Inst::load(*ty, mem.clone(), dst_old, ExtKind::ZeroExtend);
            i1.emit(sink, info, state);

            // again:
            sink.bind_label(again_label, state.ctrl_plane_mut());

            // movq %rax, %r_temp
            let i2 = Inst::mov_r_r(OperandSize::Size64, dst_old.to_reg(), temp);
            i2.emit(sink, info, state);

            use AtomicRmwSeqOp as RmwOp;
            match op {
                RmwOp::Nand => {
                    // andq %r_operand, %r_temp
                    Inst::External {
                        inst: asm::inst::andq_rm::new(temp, operand).into(),
                    }
                    .emit(sink, info, state);

                    // notq %r_temp
                    Inst::External {
                        inst: asm::inst::notq_m::new(temp).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Umin | RmwOp::Umax | RmwOp::Smin | RmwOp::Smax => {
                    // cmp %r_temp, %r_operand
                    let i3 = Inst::cmp_rmi_r(
                        OperandSize::from_ty(*ty),
                        operand,
                        RegMemImm::reg(temp.to_reg()),
                    );
                    i3.emit(sink, info, state);

                    // cmovcc %r_operand, %r_temp
                    let cc = match op {
                        RmwOp::Umin => CC::BE,
                        RmwOp::Umax => CC::NB,
                        RmwOp::Smin => CC::LE,
                        RmwOp::Smax => CC::NL,
                        _ => unreachable!(),
                    };
                    let i4 = Inst::cmove(OperandSize::Size64, cc, RegMem::reg(operand), temp);
                    i4.emit(sink, info, state);
                }
                RmwOp::And => {
                    // andq %r_operand, %r_temp
                    Inst::External {
                        inst: asm::inst::andq_rm::new(temp, operand).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Or => {
                    // orq %r_operand, %r_temp
                    Inst::External {
                        inst: asm::inst::orq_rm::new(temp, operand).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Xor => {
                    // xorq %r_operand, %r_temp
                    Inst::External {
                        inst: asm::inst::xorq_rm::new(temp, operand).into(),
                    }
                    .emit(sink, info, state);
                }
            }

            // lock cmpxchg{b,w,l,q} %r_temp, (%r_address)
            // No need to call `add_trap` here, since the `i4` emit will do that.
            let i4 = Inst::LockCmpxchg {
                ty: *ty,
                replacement: temp.to_reg(),
                expected: dst_old.to_reg(),
                mem: mem.into(),
                dst_old,
            };
            i4.emit(sink, info, state);

            // jnz again
            one_way_jmp(sink, CC::NZ, again_label);
        }

        Inst::Atomic128RmwSeq {
            op,
            mem,
            operand_low,
            operand_high,
            temp_low,
            temp_high,
            dst_old_low,
            dst_old_high,
        } => {
            let operand_low = *operand_low;
            let operand_high = *operand_high;
            let temp_low = *temp_low;
            let temp_high = *temp_high;
            let dst_old_low = *dst_old_low;
            let dst_old_high = *dst_old_high;
            debug_assert_eq!(temp_low.to_reg(), regs::rbx());
            debug_assert_eq!(temp_high.to_reg(), regs::rcx());
            debug_assert_eq!(dst_old_low.to_reg(), regs::rax());
            debug_assert_eq!(dst_old_high.to_reg(), regs::rdx());
            let mem = mem.finalize(state.frame_layout(), sink).clone();

            let again_label = sink.get_label();

            // Load the initial value.
            Inst::load(types::I64, mem.clone(), dst_old_low, ExtKind::ZeroExtend)
                .emit(sink, info, state);
            Inst::load(types::I64, mem.offset(8), dst_old_high, ExtKind::ZeroExtend)
                .emit(sink, info, state);

            // again:
            sink.bind_label(again_label, state.ctrl_plane_mut());

            // Move old value to temp registers.
            Inst::mov_r_r(OperandSize::Size64, dst_old_low.to_reg(), temp_low)
                .emit(sink, info, state);
            Inst::mov_r_r(OperandSize::Size64, dst_old_high.to_reg(), temp_high)
                .emit(sink, info, state);

            // Perform the operation.
            let operand_low_rmi = RegMemImm::reg(operand_low);
            use Atomic128RmwSeqOp as RmwOp;
            match op {
                RmwOp::Nand => {
                    // temp &= operand
                    Inst::External {
                        inst: asm::inst::andq_rm::new(temp_low, operand_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::andq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);

                    // temp = !temp
                    Inst::External {
                        inst: asm::inst::notq_m::new(temp_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::notq_m::new(temp_high).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Umin | RmwOp::Umax | RmwOp::Smin | RmwOp::Smax => {
                    // Do a comparison with LHS temp and RHS operand.
                    // `cmp_rmi_r` and `alu_rmi_r` have opposite argument orders.
                    Inst::cmp_rmi_r(OperandSize::Size64, temp_low.to_reg(), operand_low_rmi)
                        .emit(sink, info, state);
                    // This will clobber `temp_high`
                    Inst::External {
                        inst: asm::inst::sbbq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);
                    // Restore the clobbered value
                    Inst::mov_r_r(OperandSize::Size64, dst_old_high.to_reg(), temp_high)
                        .emit(sink, info, state);
                    let cc = match op {
                        RmwOp::Umin => CC::NB,
                        RmwOp::Umax => CC::B,
                        RmwOp::Smin => CC::NL,
                        RmwOp::Smax => CC::L,
                        _ => unreachable!(),
                    };
                    Inst::cmove(OperandSize::Size64, cc, operand_low.into(), temp_low)
                        .emit(sink, info, state);
                    Inst::cmove(OperandSize::Size64, cc, operand_high.into(), temp_high)
                        .emit(sink, info, state);
                }
                RmwOp::Add => {
                    Inst::External {
                        inst: asm::inst::addq_rm::new(temp_low, operand_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::adcq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Sub => {
                    Inst::External {
                        inst: asm::inst::subq_rm::new(temp_low, operand_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::sbbq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::And => {
                    Inst::External {
                        inst: asm::inst::andq_rm::new(temp_low, operand_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::andq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Or => {
                    Inst::External {
                        inst: asm::inst::orq_rm::new(temp_low, operand_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::orq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);
                }
                RmwOp::Xor => {
                    Inst::External {
                        inst: asm::inst::xorq_rm::new(temp_low, operand_low).into(),
                    }
                    .emit(sink, info, state);
                    Inst::External {
                        inst: asm::inst::xorq_rm::new(temp_high, operand_high).into(),
                    }
                    .emit(sink, info, state);
                }
            }

            // cmpxchg16b (mem)
            Inst::LockCmpxchg16b {
                replacement_low: temp_low.to_reg(),
                replacement_high: temp_high.to_reg(),
                expected_low: dst_old_low.to_reg(),
                expected_high: dst_old_high.to_reg(),
                mem: Box::new(mem.into()),
                dst_old_low,
                dst_old_high,
            }
            .emit(sink, info, state);

            // jnz again
            one_way_jmp(sink, CC::NZ, again_label);
        }

        Inst::Atomic128XchgSeq {
            mem,
            operand_low,
            operand_high,
            dst_old_low,
            dst_old_high,
        } => {
            let operand_low = *operand_low;
            let operand_high = *operand_high;
            let dst_old_low = *dst_old_low;
            let dst_old_high = *dst_old_high;
            debug_assert_eq!(operand_low, regs::rbx());
            debug_assert_eq!(operand_high, regs::rcx());
            debug_assert_eq!(dst_old_low.to_reg(), regs::rax());
            debug_assert_eq!(dst_old_high.to_reg(), regs::rdx());
            let mem = mem.finalize(state.frame_layout(), sink).clone();

            let again_label = sink.get_label();

            // Load the initial value.
            Inst::load(types::I64, mem.clone(), dst_old_low, ExtKind::ZeroExtend)
                .emit(sink, info, state);
            Inst::load(types::I64, mem.offset(8), dst_old_high, ExtKind::ZeroExtend)
                .emit(sink, info, state);

            // again:
            sink.bind_label(again_label, state.ctrl_plane_mut());

            // cmpxchg16b (mem)
            Inst::LockCmpxchg16b {
                replacement_low: operand_low,
                replacement_high: operand_high,
                expected_low: dst_old_low.to_reg(),
                expected_high: dst_old_high.to_reg(),
                mem: Box::new(mem.into()),
                dst_old_low,
                dst_old_high,
            }
            .emit(sink, info, state);

            // jnz again
            one_way_jmp(sink, CC::NZ, again_label);
        }

        Inst::Fence { kind } => {
            sink.put1(0x0F);
            sink.put1(0xAE);
            match kind {
                FenceKind::MFence => sink.put1(0xF0), // mfence = 0F AE F0
                FenceKind::LFence => sink.put1(0xE8), // lfence = 0F AE E8
                FenceKind::SFence => sink.put1(0xF8), // sfence = 0F AE F8
            }
        }

        Inst::Hlt => {
            sink.put1(0xcc);
        }

        Inst::Ud2 { trap_code } => {
            sink.add_trap(*trap_code);
            sink.put_data(Inst::TRAP_OPCODE);
        }

        Inst::Nop { len } => {
            // These encodings can all be found in Intel's architecture manual, at the NOP
            // instruction description.
            let mut len = *len;
            while len != 0 {
                let emitted = u8::min(len, 9);
                match emitted {
                    0 => {}
                    1 => sink.put1(0x90), // NOP
                    2 => {
                        // 66 NOP
                        sink.put1(0x66);
                        sink.put1(0x90);
                    }
                    3 => {
                        // NOP [EAX]
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x00);
                    }
                    4 => {
                        // NOP 0(EAX), with 0 a 1-byte immediate.
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x40);
                        sink.put1(0x00);
                    }
                    5 => {
                        // NOP [EAX, EAX, 1]
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x44);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    6 => {
                        // 66 NOP [EAX, EAX, 1]
                        sink.put1(0x66);
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x44);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    7 => {
                        // NOP 0[EAX], but 0 is a 4 bytes immediate.
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x80);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    8 => {
                        // NOP 0[EAX, EAX, 1], with 0 a 4 bytes immediate.
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x84);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    9 => {
                        // 66 NOP 0[EAX, EAX, 1], with 0 a 4 bytes immediate.
                        sink.put1(0x66);
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x84);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    _ => unreachable!(),
                }
                len -= emitted;
            }
        }

        Inst::ElfTlsGetAddr { symbol, dst } => {
            let dst = dst.to_reg().to_reg();
            debug_assert_eq!(dst, regs::rax());

            // N.B.: Must be exactly this byte sequence; the linker requires it,
            // because it must know how to rewrite the bytes.

            // data16 lea gv@tlsgd(%rip),%rdi
            sink.put1(0x66); // data16
            sink.put1(0b01001000); // REX.W
            sink.put1(0x8d); // LEA
            sink.put1(0x3d); // ModRM byte
            emit_reloc(sink, Reloc::ElfX86_64TlsGd, symbol, -4);
            sink.put4(0); // offset

            // data16 data16 callq __tls_get_addr-4
            sink.put1(0x66); // data16
            sink.put1(0x66); // data16
            sink.put1(0b01001000); // REX.W
            sink.put1(0xe8); // CALL
            emit_reloc(
                sink,
                Reloc::X86CallPLTRel4,
                &ExternalName::LibCall(LibCall::ElfTlsGetAddr),
                -4,
            );
            sink.put4(0); // offset
        }

        Inst::MachOTlsGetAddr { symbol, dst } => {
            let dst = dst.to_reg().to_reg();
            debug_assert_eq!(dst, regs::rax());

            // movq gv@tlv(%rip), %rdi
            sink.put1(0x48); // REX.w
            sink.put1(0x8b); // MOV
            sink.put1(0x3d); // ModRM byte
            emit_reloc(sink, Reloc::MachOX86_64Tlv, symbol, -4);
            sink.put4(0); // offset

            // callq *(%rdi)
            sink.put1(0xff);
            sink.put1(0x17);
        }

        Inst::CoffTlsGetAddr { symbol, dst, tmp } => {
            let dst = dst.to_reg().to_reg();
            debug_assert_eq!(dst, regs::rax());

            // tmp is used below directly as %rcx
            let tmp = tmp.to_reg().to_reg();
            debug_assert_eq!(tmp, regs::rcx());

            // See: https://gcc.godbolt.org/z/M8or9x6ss
            // And: https://github.com/bjorn3/rustc_codegen_cranelift/issues/388#issuecomment-532930282

            // Emit the following sequence
            // movl	(%rip), %eax          ; IMAGE_REL_AMD64_REL32	_tls_index
            // movq	%gs:88, %rcx
            // movq	(%rcx,%rax,8), %rax
            // leaq	(%rax), %rax          ; Reloc: IMAGE_REL_AMD64_SECREL	symbol

            // Load TLS index for current thread
            // movl	(%rip), %eax
            sink.put1(0x8b); // mov
            sink.put1(0x05);
            emit_reloc(
                sink,
                Reloc::X86PCRel4,
                &ExternalName::KnownSymbol(KnownSymbol::CoffTlsIndex),
                -4,
            );
            sink.put4(0); // offset

            // movq	%gs:88, %rcx
            // Load the TLS Storage Array pointer
            // The gs segment register refers to the base address of the TEB on x64.
            // 0x58 is the offset in the TEB for the ThreadLocalStoragePointer member on x64:
            sink.put_data(&[
                0x65, 0x48, // REX.W
                0x8b, // MOV
                0x0c, 0x25, 0x58, // 0x58 - ThreadLocalStoragePointer offset
                0x00, 0x00, 0x00,
            ]);

            // movq	(%rcx,%rax,8), %rax
            // Load the actual TLS entry for this thread.
            // Computes ThreadLocalStoragePointer + _tls_index*8
            sink.put_data(&[0x48, 0x8b, 0x04, 0xc1]);

            // leaq	(%rax), %rax
            sink.put1(0x48);
            sink.put1(0x8d);
            sink.put1(0x80);
            emit_reloc(sink, Reloc::X86SecRel, symbol, 0);
            sink.put4(0); // offset
        }

        Inst::Unwind { inst } => {
            sink.add_unwind(inst.clone());
        }

        Inst::DummyUse { .. } => {
            // Nothing.
        }

        Inst::External { inst } => {
            let mut known_offsets = [0, 0];
            // These values are transcribed from what is happening in
            // `SyntheticAmode::finalize`. This, plus the `Into` logic
            // converting a `SyntheticAmode` to its external counterpart, are
            // necessary to communicate Cranelift's internal offsets to the
            // assembler; due to when Cranelift determines these offsets, this
            // happens quite late (i.e., here during emission).
            let frame = state.frame_layout();
            known_offsets[usize::from(external::offsets::KEY_INCOMING_ARG)] =
                i32::try_from(frame.tail_args_size + frame.setup_area_size).unwrap();
            known_offsets[usize::from(external::offsets::KEY_SLOT_OFFSET)] =
                i32::try_from(frame.outgoing_args_size).unwrap();
            inst.encode(sink, &known_offsets);
        }
    }

    state.clear_post_insn();
}

/// Emit the common sequence used for both direct and indirect tail calls:
///
/// * Copy the new frame's stack arguments over the top of our current frame.
///
/// * Restore the old frame pointer.
///
/// * Initialize the tail callee's stack pointer (simultaneously deallocating
///   the temporary stack space we allocated when creating the new frame's stack
///   arguments).
///
/// * Move the return address into its stack slot.
fn emit_return_call_common_sequence<T>(
    sink: &mut MachBuffer<Inst>,
    info: &EmitInfo,
    state: &mut EmitState,
    call_info: &ReturnCallInfo<T>,
) {
    assert!(
        info.flags.preserve_frame_pointers(),
        "frame pointers aren't fundamentally required for tail calls, \
                 but the current implementation relies on them being present"
    );

    let tmp = call_info.tmp.to_writable_reg();

    for inst in
        X64ABIMachineSpec::gen_clobber_restore(CallConv::Tail, &info.flags, state.frame_layout())
    {
        inst.emit(sink, info, state);
    }

    for inst in X64ABIMachineSpec::gen_epilogue_frame_restore(
        CallConv::Tail,
        &info.flags,
        &info.isa_flags,
        state.frame_layout(),
    ) {
        inst.emit(sink, info, state);
    }

    let incoming_args_diff = state.frame_layout().tail_args_size - call_info.new_stack_arg_size;
    if incoming_args_diff > 0 {
        // Move the saved return address up by `incoming_args_diff`.
        let addr = Amode::imm_reg(0, regs::rsp());
        let inst = asm::inst::movq_rm::new(tmp, addr).into();
        Inst::External { inst }.emit(sink, info, state);
        Inst::mov_r_m(
            OperandSize::Size64,
            tmp.to_reg(),
            Amode::imm_reg(i32::try_from(incoming_args_diff).unwrap(), regs::rsp()),
        )
        .emit(sink, info, state);

        // Increment the stack pointer to shrink the argument area for the new
        // call.
        let rsp = Writable::from_reg(regs::rsp());
        let incoming_args_diff = i32::try_from(incoming_args_diff)
            .expect("`incoming_args_diff` is too large to fit in a 32-bit signed immediate");
        Inst::addq_mi(rsp, incoming_args_diff).emit(sink, info, state);
    }
}
