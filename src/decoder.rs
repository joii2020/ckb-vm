use ckb_vm_definitions::instructions::{self as insts};
use ckb_vm_definitions::registers::{RA, ZERO};

use crate::instructions::{
    a, b, extract_opcode, i, instruction_length, m, rvc, set_instruction_length_n, Instruction,
    InstructionFactory, Itype, R4type, R5type, Register, Rtype, Utype,
};
use crate::machine::VERSION2;
use crate::memory::Memory;
use crate::{Error, ISA_A, ISA_B, ISA_MOP, RISCV_MAX_MEMORY, RISCV_PAGESIZE};

const RISCV_PAGESIZE_MASK: u64 = RISCV_PAGESIZE as u64 - 1;
const INSTRUCTION_CACHE_SIZE: usize = 4096;

pub struct Decoder {
    factories: Vec<InstructionFactory>,
    mop: bool,
    version: u32,
    // use a cache of instructions to avoid decoding the same instruction twice, pc is the key and the instruction is the value
    instructions_cache: [(u64, u64); INSTRUCTION_CACHE_SIZE],
}

impl Decoder {
    pub fn new(mop: bool, version: u32) -> Decoder {
        Decoder {
            factories: vec![],
            mop,
            version,
            instructions_cache: [(RISCV_MAX_MEMORY as u64, 0); INSTRUCTION_CACHE_SIZE],
        }
    }

    pub fn add_instruction_factory(&mut self, factory: InstructionFactory) {
        self.factories.push(factory);
    }

    // This method is used to decode instruction raw bits from memory pointed
    // by current PC. Right now we support 32-bit instructions and RVC compressed
    // instructions. In future version we might add support for longer instructions.
    //
    // This decode method actually leverages a trick from little endian encoding:
    // the format for a full 32 bit RISC-V instruction is as follows:
    //
    // WWWWWWWWZZZZZZZZYYYYYYYYXXXXXX11
    //
    // While the format for a 16 bit RVC RIST-V instruction is one of the following 3:
    //
    // YYYYYYYYXXXXXX00
    // YYYYYYYYXXXXXX01
    // YYYYYYYYXXXXXX10
    //
    // Here X, Y, Z and W stands for arbitrary bits.
    // However the above is the representation in a 16-bit or 32-bit integer, since
    // we are using little endian, in memory it's actually in following reversed order:
    //
    // XXXXXX11 YYYYYYYY ZZZZZZZZ WWWWWWWW
    // XXXXXX00 YYYYYYYY
    // XXXXXX01 YYYYYYYY
    // XXXXXX10 YYYYYYYY
    //
    // One observation here, is the first byte in memory is always the least
    // significant byte whether we load a 32-bit or 16-bit integer.
    // So when we are decoding an instruction, we can first load 2 bytes forming
    // a 16-bit integer, then we check the 2 least significant bits, if the 2 bitss
    // are 0b11, we know this is a 32-bit instruction, we should load another 2 bytes
    // from memory and concat the 2 16-bit integers into a full 32-bit integers.
    // Otherwise, we know we are loading a RVC integer, and we are done here.
    // Also, due to RISC-V encoding behavior, it's totally okay when we cast a 16-bit
    // RVC instruction into a 32-bit instruction, the meaning of the instruction stays
    // unchanged in the cast conversion.
    fn decode_bits<M: Memory>(&self, memory: &mut M, pc: u64) -> Result<u32, Error> {
        // when the address is not the last 2 bytes of an executable page,
        // use a faster path to load instruction bits
        if pc & RISCV_PAGESIZE_MASK < RISCV_PAGESIZE_MASK - 1 {
            let mut instruction_bits = memory.execute_load32(pc)?;
            if instruction_bits & 0x3 != 0x3 {
                instruction_bits &= 0xffff;
            }
            Ok(instruction_bits)
        } else {
            let mut instruction_bits = u32::from(memory.execute_load16(pc)?);
            if instruction_bits & 0x3 == 0x3 {
                instruction_bits |= u32::from(memory.execute_load16(pc + 2)?) << 16;
            }
            Ok(instruction_bits)
        }
    }

    pub fn decode_raw<M: Memory>(&mut self, memory: &mut M, pc: u64) -> Result<Instruction, Error> {
        // since we are using RISCV_MAX_MEMORY as the default key in the instruction cache, have to check out of bound error first
        if pc as usize >= RISCV_MAX_MEMORY {
            return Err(Error::MemOutOfBound);
        }
        let instruction_cache_key = {
            // according to RISC-V instruction encoding, the lowest bit in PC will always be zero
            let pc = pc >> 1;
            // Here we try to balance between local code and remote code. At times,
            // we can find the code jumping to a remote function(e.g., memcpy or
            // alloc), then resumes execution at a local location. Previous cache
            // key only optimizes for local operations, while this new cache key
            // balances the code between a 8192-byte local region, and certain remote
            // code region. Notice the value 12 and 8 here are chosen by empirical
            // evidence.
            ((pc & 0xFF) | (pc >> 12 << 8)) as usize % INSTRUCTION_CACHE_SIZE
        };
        let cached_instruction = self.instructions_cache[instruction_cache_key];
        if cached_instruction.0 == pc {
            return Ok(cached_instruction.1);
        }
        let instruction_bits = self.decode_bits(memory, pc)?;
        for factory in &self.factories {
            if let Some(instruction) = factory(instruction_bits, self.version) {
                self.instructions_cache[instruction_cache_key] = (pc, instruction);
                return Ok(instruction);
            }
        }
        Err(Error::InvalidInstruction {
            pc,
            instruction: instruction_bits,
        })
    }

    // Macro-Operation Fusion (also Macro-Op Fusion, MOP Fusion, or Macrofusion) is a hardware optimization technique found
    // in many modern microarchitectures whereby a series of adjacent macro-operations are merged into a single
    // macro-operation prior or during decoding. Those instructions are later decoded into fused-µOPs.
    //
    // - https://riscv.org/wp-content/uploads/2016/07/Tue1130celio-fusion-finalV2.pdf
    // - https://en.wikichip.org/wiki/macro-operation_fusion#Proposed_fusion_operations
    // - https://carrv.github.io/2017/papers/clark-rv8-carrv2017.pdf
    pub fn decode_mop<M: Memory>(&mut self, memory: &mut M, pc: u64) -> Result<Instruction, Error> {
        let head_instruction = self.decode_raw(memory, pc)?;
        let head_opcode = extract_opcode(head_instruction);
        match head_opcode {
            insts::OP_ADD => {
                let rule_adc = |decoder: &mut Self,
                                memory: &mut M|
                 -> Result<Option<Instruction>, Error> {
                    let head_inst = Rtype(head_instruction);
                    let head_size = instruction_length(head_instruction);
                    if head_inst.rd() != head_inst.rs1() || head_inst.rs1() == head_inst.rs2() {
                        return Ok(None);
                    }
                    let next_instruction = decoder.decode_raw(memory, pc + head_size as u64)?;
                    let next_opcode = extract_opcode(next_instruction);
                    if next_opcode != insts::OP_SLTU {
                        return Ok(None);
                    }
                    let next_inst = Rtype(next_instruction);
                    let next_size = instruction_length(next_instruction);
                    if next_inst.rd() != head_inst.rs2()
                        || head_inst.rs2() != next_inst.rs2()
                        || next_inst.rs1() != head_inst.rs1()
                    {
                        return Ok(None);
                    }
                    let neck_instruction =
                        decoder.decode_raw(memory, pc + head_size as u64 + next_size as u64)?;
                    let neck_opcode = extract_opcode(neck_instruction);
                    if neck_opcode != insts::OP_ADD {
                        return Ok(None);
                    }
                    let neck_inst = Rtype(neck_instruction);
                    let neck_size = instruction_length(neck_instruction);
                    if neck_inst.rd() != neck_inst.rs1()
                        || neck_inst.rs1() != next_inst.rs1()
                        || neck_inst.rs2() == head_inst.rs1()
                        || neck_inst.rs2() == head_inst.rs2()
                    {
                        return Ok(None);
                    }
                    let body_instruction = decoder.decode_raw(
                        memory,
                        pc + head_size as u64 + next_size as u64 + neck_size as u64,
                    )?;
                    let body_opcode = extract_opcode(body_instruction);
                    if body_opcode != insts::OP_SLTU {
                        return Ok(None);
                    }
                    let body_inst = Rtype(body_instruction);
                    let body_size = instruction_length(body_instruction);
                    if body_inst.rd() != body_inst.rs2()
                        || body_inst.rs2() != neck_inst.rs2()
                        || body_inst.rs1() != neck_inst.rs1()
                    {
                        return Ok(None);
                    }
                    let tail_instruction = decoder.decode_raw(
                        memory,
                        pc + head_size as u64
                            + next_size as u64
                            + neck_size as u64
                            + body_size as u64,
                    )?;
                    let tail_opcode = extract_opcode(tail_instruction);
                    if tail_opcode != insts::OP_OR {
                        return Ok(None);
                    }
                    let tail_inst = Rtype(tail_instruction);
                    let tail_size = instruction_length(tail_instruction);
                    if tail_inst.rd() != tail_inst.rs1()
                        || tail_inst.rs1() != head_inst.rs2()
                        || tail_inst.rs2() != body_inst.rs2()
                    {
                        return Ok(None);
                    }
                    if head_inst.rd() == ZERO || next_inst.rd() == ZERO || body_inst.rd() == ZERO {
                        return Ok(None);
                    }
                    let fuze_inst = Rtype::new(
                        insts::OP_ADC,
                        head_inst.rd(),
                        next_inst.rd(),
                        body_inst.rd(),
                    );
                    let fuze_size = head_size + next_size + neck_size + body_size + tail_size;
                    Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)))
                };
                let rule_add3 =
                    |decoder: &mut Self, memory: &mut M| -> Result<Option<Instruction>, Error> {
                        if decoder.version < VERSION2 {
                            return Ok(None);
                        }

                        let i0 = Rtype(head_instruction);
                        let i0_size = instruction_length(head_instruction);

                        let (i1, i1_size) = {
                            let i1 = decoder.decode_raw(memory, pc + i0_size as u64)?;
                            let i1_opcode = extract_opcode(i1);
                            if i1_opcode != insts::OP_SLTU {
                                return Ok(None);
                            }
                            (Rtype(i1), instruction_length(i1))
                        };

                        let (i2, i2_size) = {
                            let i2 =
                                decoder.decode_raw(memory, pc + i0_size as u64 + i1_size as u64)?;
                            let i2_opcode = extract_opcode(i2);
                            if i2_opcode != insts::OP_ADD {
                                return Ok(None);
                            }
                            (Rtype(i2), instruction_length(i2))
                        };

                        let fuze_size = i0_size + i1_size + i2_size;

                        {
                            // add r0, r1, r0
                            // sltu r2, r0, r1
                            // add r3, r2, r4
                            //
                            // r0 != r1
                            // r0 != r4
                            // r2 != r4
                            // r0 != x0
                            // r2 != x0
                            let r0 = i0.rd();
                            let r1 = i0.rs1();
                            let r2 = i1.rd();
                            let r3 = i2.rd();
                            let r4 = i2.rs2();

                            if i0.rd() == r0
                                && i0.rs1() == r1
                                && i0.rs2() == r0
                                && i1.rd() == r2
                                && i1.rs1() == r0
                                && i1.rs2() == r1
                                && i2.rd() == r3
                                && i2.rs1() == r2
                                && i2.rs2() == r4
                                && r0 != r1
                                && r0 != r4
                                && r2 != r4
                                && r0 != ZERO
                                && r2 != ZERO
                            {
                                let fuze_inst = R5type::new(insts::OP_ADD3A, r0, r1, r2, r3, r4);
                                return Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)));
                            }
                        }

                        {
                            // add r0, r1, r2
                            // sltu r1, r0, r1
                            // add r3, r1, r4
                            //
                            // r0 != r1
                            // r0 != r4
                            // r1 != r4
                            // r0 != x0
                            // r1 != x0
                            let r0 = i0.rd();
                            let r1 = i0.rs1();
                            let r2 = i0.rs2();
                            let r3 = i2.rd();
                            let r4 = i2.rs2();

                            if i0.rd() == r0
                                && i0.rs1() == r1
                                && i0.rs2() == r2
                                && i1.rd() == r1
                                && i1.rs1() == r0
                                && i1.rs2() == r1
                                && i2.rd() == r3
                                && i2.rs1() == r1
                                && i2.rs2() == r4
                                && r0 != r1
                                && r0 != r4
                                && r1 != r4
                                && r0 != ZERO
                                && r1 != ZERO
                            {
                                let fuze_inst = R5type::new(insts::OP_ADD3B, r0, r1, r2, r3, r4);
                                return Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)));
                            }
                        }
                        {
                            // add r0, r1, r2
                            // sltu r3, r0, r1
                            // add r3, r3, r4
                            //
                            // r0 != r1
                            // r0 != r4
                            // r3 != r4
                            // r0 != x0
                            // r3 != x0
                            let r0 = i0.rd();
                            let r1 = i0.rs1();
                            let r2 = i0.rs2();
                            let r3 = i1.rd();
                            let r4 = i2.rs2();

                            if i0.rd() == r0
                                && i0.rs1() == r1
                                && i0.rs2() == r2
                                && i1.rd() == r3
                                && i1.rs1() == r0
                                && i1.rs2() == r1
                                && i2.rd() == r3
                                && i2.rs1() == r3
                                && i2.rs2() == r4
                                && r0 != r1
                                && r0 != r4
                                && r3 != r4
                                && r0 != ZERO
                                && r3 != ZERO
                            {
                                let fuze_inst = R5type::new(insts::OP_ADD3C, r0, r1, r2, r3, r4);
                                return Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)));
                            }
                        }
                        Ok(None)
                    };
                let rule_adcs =
                    |decoder: &mut Self, memory: &mut M| -> Result<Option<Instruction>, Error> {
                        // add r0, r1, r2
                        // sltu r3, r0, r1
                        //
                        // or
                        //
                        // add r0, r2, r1
                        // sltu r3, r0, r1
                        //
                        // r0 != r1
                        // r0 != x0
                        if decoder.version < VERSION2 {
                            return Ok(None);
                        }

                        let mut i0 = Rtype(head_instruction);
                        let i0_size = instruction_length(head_instruction);

                        if i0.rd() == i0.rs1() && i0.rd() != i0.rs2() {
                            i0 = Rtype::new(i0.op(), i0.rd(), i0.rs2(), i0.rs1());
                        }

                        let (i1, i1_size) = {
                            let i1 = decoder.decode_raw(memory, pc + i0_size as u64)?;
                            let i1_opcode = extract_opcode(i1);
                            if i1_opcode != insts::OP_SLTU {
                                return Ok(None);
                            }
                            (Rtype(i1), instruction_length(i1))
                        };

                        let r0 = i0.rd();
                        let r1 = i0.rs1();
                        let r2 = i0.rs2();
                        let r3 = i1.rd();

                        if i0.rd() == r0
                            && i0.rs1() == r1
                            && i0.rs2() == r2
                            && i1.rd() == r3
                            && i1.rs1() == r0
                            && i1.rs2() == r1
                            && r0 != r1
                            && r0 != ZERO
                        {
                            let fuze_inst = R4type::new(insts::OP_ADCS, r0, r1, r2, r3);
                            let fuze_size = i0_size + i1_size;
                            Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)))
                        } else {
                            Ok(None)
                        }
                    };
                if let Ok(Some(i)) = rule_adc(self, memory) {
                    Ok(i)
                } else if let Ok(Some(i)) = rule_add3(self, memory) {
                    Ok(i)
                } else if let Ok(Some(i)) = rule_adcs(self, memory) {
                    Ok(i)
                } else {
                    Ok(head_instruction)
                }
            }
            insts::OP_SUB => {
                let rule_sbb =
                    |decoder: &mut Self, memory: &mut M| -> Result<Option<Instruction>, Error> {
                        let head_inst = Rtype(head_instruction);
                        let head_size = instruction_length(head_instruction);
                        if head_inst.rd() != head_inst.rs2() || head_inst.rs1() == head_inst.rs2() {
                            return Ok(None);
                        }
                        let next_instruction = decoder.decode_raw(memory, pc + head_size as u64)?;
                        let next_opcode = extract_opcode(next_instruction);
                        if next_opcode != insts::OP_SLTU {
                            return Ok(None);
                        }
                        let next_inst = Rtype(next_instruction);
                        let next_size = instruction_length(next_instruction);
                        if next_inst.rd() == head_inst.rs1()
                            || next_inst.rd() == head_inst.rs2()
                            || next_inst.rs1() != head_inst.rs1()
                            || next_inst.rs2() != next_inst.rs2()
                        {
                            return Ok(None);
                        }
                        let neck_instruction =
                            decoder.decode_raw(memory, pc + head_size as u64 + next_size as u64)?;
                        let neck_opcode = extract_opcode(neck_instruction);
                        if neck_opcode != insts::OP_SUB {
                            return Ok(None);
                        }
                        let neck_inst = Rtype(neck_instruction);
                        let neck_size = instruction_length(neck_instruction);
                        if neck_inst.rd() != head_inst.rs1()
                            || neck_inst.rs1() != head_inst.rs2()
                            || neck_inst.rs2() == head_inst.rs1()
                            || neck_inst.rs2() == head_inst.rs2()
                            || neck_inst.rs2() == next_inst.rd()
                        {
                            return Ok(None);
                        }
                        let body_instruction = decoder.decode_raw(
                            memory,
                            pc + head_size as u64 + next_size as u64 + neck_size as u64,
                        )?;
                        let body_opcode = extract_opcode(body_instruction);
                        if body_opcode != insts::OP_SLTU {
                            return Ok(None);
                        }
                        let body_inst = Rtype(body_instruction);
                        let body_size = instruction_length(body_instruction);
                        if body_inst.rd() != neck_inst.rs2()
                            || body_inst.rs1() != head_inst.rs2()
                            || body_inst.rs2() != head_inst.rs1()
                        {
                            return Ok(None);
                        }
                        let tail_instruction = decoder.decode_raw(
                            memory,
                            pc + head_size as u64
                                + next_size as u64
                                + neck_size as u64
                                + body_size as u64,
                        )?;
                        let tail_opcode = extract_opcode(tail_instruction);
                        if tail_opcode != insts::OP_OR {
                            return Ok(None);
                        }
                        let tail_inst = Rtype(tail_instruction);
                        let tail_size = instruction_length(tail_instruction);
                        if tail_inst.rd() != head_inst.rd()
                            || tail_inst.rs1() != neck_inst.rs2()
                            || tail_inst.rs2() != next_inst.rd()
                        {
                            return Ok(None);
                        }
                        let fuze_inst = R4type::new(
                            insts::OP_SBB,
                            head_inst.rs1(),
                            head_inst.rs2(),
                            neck_inst.rs2(),
                            next_inst.rd(),
                        );
                        if head_inst.rs1() == ZERO
                            || head_inst.rs2() == ZERO
                            || neck_inst.rs2() == ZERO
                            || next_inst.rd() == ZERO
                        {
                            return Ok(None);
                        }
                        let fuze_size = head_size + next_size + neck_size + body_size + tail_size;
                        Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)))
                    };
                let rule_sbbs =
                    |decoder: &mut Self, memory: &mut M| -> Result<Option<Instruction>, Error> {
                        // sub r0, r1, r2
                        // sltu r3, r1, r2
                        //
                        // r0 != r1
                        // r0 != r2
                        if decoder.version < VERSION2 {
                            return Ok(None);
                        }

                        let i0 = Rtype(head_instruction);
                        let i0_size = instruction_length(head_instruction);

                        let (i1, i1_size) = {
                            let i1 = decoder.decode_raw(memory, pc + i0_size as u64)?;
                            let i1_opcode = extract_opcode(i1);
                            if i1_opcode != insts::OP_SLTU {
                                return Ok(None);
                            }
                            (Rtype(i1), instruction_length(i1))
                        };

                        let r0 = i0.rd();
                        let r1 = i0.rs1();
                        let r2 = i0.rs2();
                        let r3 = i1.rd();

                        if i0.rd() == r0
                            && i0.rs1() == r1
                            && i0.rs2() == r2
                            && i1.rd() == r3
                            && i1.rs1() == r1
                            && i1.rs2() == r2
                            && r0 != r1
                            && r0 != r2
                        {
                            let fuze_inst = R4type::new(insts::OP_SBBS, r0, r1, r2, r3);
                            let fuze_size = i0_size + i1_size;
                            Ok(Some(set_instruction_length_n(fuze_inst.0, fuze_size)))
                        } else {
                            Ok(None)
                        }
                    };
                if let Ok(Some(i)) = rule_sbb(self, memory) {
                    Ok(i)
                } else if let Ok(Some(i)) = rule_sbbs(self, memory) {
                    Ok(i)
                } else {
                    Ok(head_instruction)
                }
            }
            insts::OP_LUI => {
                let head_inst = Utype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_JALR_VERSION1 => {
                        let next_inst = Itype(next_instruction);
                        let test_condition = if self.version >= VERSION2 {
                            next_inst.rs1() == head_inst.rd()
                                && next_inst.rd() == RA
                                && next_inst.rs1() == RA
                        } else {
                            next_inst.rs1() == head_inst.rd() && next_inst.rd() == RA
                        };
                        if test_condition {
                            let fuze_imm = head_inst
                                .immediate_s()
                                .wrapping_add(next_inst.immediate_s());
                            let fuze_inst = Utype::new_s(insts::OP_FAR_JUMP_ABS, RA, fuze_imm);
                            let next_size = instruction_length(next_instruction);
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    insts::OP_ADDIW => {
                        let next_inst = Itype(next_instruction);
                        if next_inst.rs1() == next_inst.rd() && next_inst.rd() == head_inst.rd() {
                            let fuze_imm = head_inst
                                .immediate_s()
                                .wrapping_add(next_inst.immediate_s());
                            let fuze_inst =
                                Utype::new_s(insts::OP_CUSTOM_LOAD_IMM, head_inst.rd(), fuze_imm);
                            let next_size = instruction_length(next_instruction);
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    _ => Ok(head_instruction),
                }
            }
            insts::OP_AUIPC => {
                let head_inst = Utype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_JALR_VERSION1 => {
                        let next_inst = Itype(next_instruction);
                        let mut result = head_instruction;

                        if self.version >= VERSION2 {
                            if next_inst.rs1() == head_inst.rd()
                                && next_inst.rd() == RA
                                && next_inst.rs1() == RA
                            {
                                if let Some(fuze_imm) =
                                    head_inst.immediate_s().checked_add(next_inst.immediate_s())
                                {
                                    let fuze_inst =
                                        Utype::new_s(insts::OP_FAR_JUMP_REL, RA, fuze_imm);
                                    let next_size = instruction_length(next_instruction);
                                    let fuze_size = head_size + next_size;
                                    result = set_instruction_length_n(fuze_inst.0, fuze_size);
                                }
                            }
                        } else {
                            if next_inst.rs1() == head_inst.rd() && next_inst.rd() == RA {
                                let fuze_imm = head_inst
                                    .immediate_s()
                                    .wrapping_add(next_inst.immediate_s());
                                let fuze_inst = Utype::new_s(insts::OP_FAR_JUMP_REL, RA, fuze_imm);
                                let next_size = instruction_length(next_instruction);
                                let fuze_size = head_size + next_size;
                                result = set_instruction_length_n(fuze_inst.0, fuze_size);
                            }
                        }
                        Ok(result)
                    }
                    insts::OP_ADDI if self.version >= VERSION2 => {
                        let next_inst = Itype(next_instruction);
                        let mut result = head_instruction;

                        if next_inst.rs1() == next_inst.rd() && next_inst.rd() == head_inst.rd() {
                            if let Ok(pc) = i32::try_from(pc) {
                                if let Some(fuze_imm) = head_inst
                                    .immediate_s()
                                    .checked_add(next_inst.immediate_s())
                                    .and_then(|s| s.checked_add(pc))
                                {
                                    let fuze_inst = Utype::new_s(
                                        insts::OP_CUSTOM_LOAD_IMM,
                                        head_inst.rd(),
                                        fuze_imm,
                                    );
                                    let next_size = instruction_length(next_instruction);
                                    let fuze_size = head_size + next_size;
                                    result = set_instruction_length_n(fuze_inst.0, fuze_size);
                                }
                            }
                        }
                        Ok(result)
                    }
                    _ => Ok(head_instruction),
                }
            }
            insts::OP_MULH => {
                let head_inst = Rtype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_MUL => {
                        let next_inst = Rtype(next_instruction);
                        if head_inst.rd() != head_inst.rs1()
                            && head_inst.rd() != head_inst.rs2()
                            && head_inst.rs1() == next_inst.rs1()
                            && head_inst.rs2() == next_inst.rs2()
                            && head_inst.rd() != next_inst.rd()
                        {
                            let next_size = instruction_length(next_instruction);
                            let fuze_inst = R4type::new(
                                insts::OP_WIDE_MUL,
                                head_inst.rd(),
                                head_inst.rs1(),
                                head_inst.rs2(),
                                next_inst.rd(),
                            );
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    _ => Ok(head_instruction),
                }
            }
            insts::OP_MULHU => {
                let head_inst = Rtype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_MUL => {
                        let next_inst = Rtype(next_instruction);
                        if head_inst.rd() != head_inst.rs1()
                            && head_inst.rd() != head_inst.rs2()
                            && head_inst.rs1() == next_inst.rs1()
                            && head_inst.rs2() == next_inst.rs2()
                            && head_inst.rd() != next_inst.rd()
                        {
                            let next_size = instruction_length(next_instruction);
                            let fuze_inst = R4type::new(
                                insts::OP_WIDE_MULU,
                                head_inst.rd(),
                                head_inst.rs1(),
                                head_inst.rs2(),
                                next_inst.rd(),
                            );
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    _ => Ok(head_instruction),
                }
            }
            insts::OP_MULHSU => {
                let head_inst = Rtype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_MUL => {
                        let next_inst = Rtype(next_instruction);
                        if head_inst.rd() != head_inst.rs1()
                            && head_inst.rd() != head_inst.rs2()
                            && head_inst.rs1() == next_inst.rs1()
                            && head_inst.rs2() == next_inst.rs2()
                            && head_inst.rd() != next_inst.rd()
                        {
                            let next_size = instruction_length(next_instruction);
                            let fuze_inst = R4type::new(
                                insts::OP_WIDE_MULSU,
                                head_inst.rd(),
                                head_inst.rs1(),
                                head_inst.rs2(),
                                next_inst.rd(),
                            );
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    _ => Ok(head_instruction),
                }
            }
            insts::OP_DIV => {
                let head_inst = Rtype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_REM => {
                        let next_inst = Rtype(next_instruction);
                        if head_inst.rd() != head_inst.rs1()
                            && head_inst.rd() != head_inst.rs2()
                            && head_inst.rs1() == next_inst.rs1()
                            && head_inst.rs2() == next_inst.rs2()
                            && head_inst.rd() != next_inst.rd()
                        {
                            let next_size = instruction_length(next_instruction);
                            let fuze_inst = R4type::new(
                                insts::OP_WIDE_DIV,
                                head_inst.rd(),
                                head_inst.rs1(),
                                head_inst.rs2(),
                                next_inst.rd(),
                            );
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    _ => Ok(head_instruction),
                }
            }
            insts::OP_DIVU => {
                let head_inst = Rtype(head_instruction);
                let head_size = instruction_length(head_instruction);
                let next_instruction = match self.decode_raw(memory, pc + head_size as u64) {
                    Ok(ni) => ni,
                    Err(_) => return Ok(head_instruction),
                };
                let next_opcode = extract_opcode(next_instruction);
                match next_opcode {
                    insts::OP_REMU => {
                        let next_inst = Rtype(next_instruction);
                        if head_inst.rd() != head_inst.rs1()
                            && head_inst.rd() != head_inst.rs2()
                            && head_inst.rs1() == next_inst.rs1()
                            && head_inst.rs2() == next_inst.rs2()
                            && head_inst.rd() != next_inst.rd()
                        {
                            let next_size = instruction_length(next_instruction);
                            let fuze_inst = R4type::new(
                                insts::OP_WIDE_DIVU,
                                head_inst.rd(),
                                head_inst.rs1(),
                                head_inst.rs2(),
                                next_inst.rd(),
                            );
                            let fuze_size = head_size + next_size;
                            Ok(set_instruction_length_n(fuze_inst.0, fuze_size))
                        } else {
                            Ok(head_instruction)
                        }
                    }
                    _ => Ok(head_instruction),
                }
            }
            _ => Ok(head_instruction),
        }
    }

    pub fn decode<M: Memory>(&mut self, memory: &mut M, pc: u64) -> Result<Instruction, Error> {
        if self.mop {
            self.decode_mop(memory, pc)
        } else {
            self.decode_raw(memory, pc)
        }
    }

    pub fn reset_instructions_cache(&mut self) {
        self.instructions_cache = [(RISCV_MAX_MEMORY as u64, 0); INSTRUCTION_CACHE_SIZE];
    }
}

pub fn build_decoder<R: Register>(isa: u8, version: u32) -> Decoder {
    let mut decoder = Decoder::new(isa & ISA_MOP != 0, version);
    decoder.add_instruction_factory(rvc::factory::<R>);
    decoder.add_instruction_factory(i::factory::<R>);
    decoder.add_instruction_factory(m::factory::<R>);
    if isa & ISA_B != 0 {
        decoder.add_instruction_factory(b::factory::<R>);
    }
    if isa & ISA_A != 0 {
        decoder.add_instruction_factory(a::factory::<R>);
    }
    decoder
}
