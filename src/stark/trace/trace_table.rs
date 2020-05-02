use crate::math::{ field, fft, polynom, parallel, quartic::to_quartic_vec };
use crate::crypto::{ MerkleTree, HashFunction };
use crate::processor::opcodes;
use crate::utils::{ uninit_vector };
use crate::stark::{ CompositionCoefficients };
use super::{ TraceState, decoder, stack, MAX_REGISTER_COUNT };

// TYPES AND INTERFACES
// ================================================================================================
pub struct TraceTable {
    registers   : Vec<Vec<u64>>,
    polys       : Vec<Vec<u64>>,
    ext_factor  : usize,
}

// TRACE TABLE IMPLEMENTATION
// ================================================================================================
impl TraceTable {

    /// Returns a trace table resulting from the execution of the specified program. Space for the
    /// trace table is allocated in accordance with the specified `extension_factor`.
    pub fn new(program: &[u64], inputs: &[u64], extension_factor: usize) -> TraceTable {
        
        assert!(program.len().is_power_of_two(), "program length must be a power of 2");
        assert!(extension_factor.is_power_of_two(), "trace extension factor must be a power of 2");
        assert!(program[program.len() - 1] == opcodes::NOOP, "last operation of a program must be NOOP");

        // create different segments of the trace
        let decoder_registers = decoder::process(program, extension_factor);
        let stack_registers = stack::execute(program, inputs, extension_factor);

        // move all trace registers into a single vector
        let mut registers = Vec::new();
        for register in decoder_registers.into_iter() { registers.push(register); }
        for register in stack_registers.into_iter() { registers.push(register); }

        assert!(registers.len() < MAX_REGISTER_COUNT,
            "execution trace cannot have more than {} registers", MAX_REGISTER_COUNT);

        let polys = Vec::with_capacity(registers.len());
        return TraceTable { registers, polys, ext_factor: extension_factor };
    }

    /// Returns state of the trace table at the specified `step`.
    pub fn get_state(&self, step: usize) -> TraceState {
        let mut result = TraceState::new(self.max_stack_depth());
        self.fill_state(&mut result, step);
        return result;
    }

    /// Copies trace table state at the specified `step` to the passed in `state` object.
    pub fn fill_state(&self, state: &mut TraceState, step: usize) {
        for i in 0..self.registers.len() {
            state.set_register(i, self.registers[i][step]);
        }
    }

    /// Returns the number of states in the un-extended trace table.
    pub fn unextended_length(&self) -> usize {
        return self.registers[0].capacity() / self.ext_factor;
    }

    /// Returns the number of states in the extended trace table.
    pub fn domain_size(&self) -> usize {
        return self.registers[0].capacity();
    }

    /// Returns `extension_factor` for the trace table.
    pub fn extension_factor(&self) -> usize {
        return self.ext_factor;
    }

    /// Returns the number of registers in the trace table.
    pub fn register_count(&self) -> usize {
        return self.registers.len();
    }

    /// Returns the number of registers used by the stack.
    pub fn max_stack_depth(&self) -> usize {
        return self.registers.len() - decoder::NUM_REGISTERS;
    }

    /// Returns trace of the register at the specified `index`.
    pub fn get_register_trace(&self, index: usize) -> &[u64] {
        return &self.registers[index];
    }

    /// Returns polynomial of the register at the specified `index`; can be called only
    /// after the trace table has been extended.
    pub fn get_register_poly(&self, index: usize) -> &[u64] {
        assert!(self.is_extended(), "trace table has not been extended yet");
        return &self.polys[index];
    }

    /// Returns trace of the stack register at the specified `index`.
    pub fn get_stack_register_trace(&self, index: usize) -> &[u64] {
        return &self.registers[index + decoder::NUM_REGISTERS];
    }

    /// Returns `true` if the trace table has been extended.
    pub fn is_extended(&self) -> bool {
        return self.registers[0].len() == self.registers[0].capacity();
    }

    /// Extends all registers of the trace table by the `extension_factor` specified during
    /// trace table construction. A trace table can be extended only once.
    pub fn extend(&mut self) {
        assert!(!self.is_extended(), "trace table has already been extended");
        let domain_size = self.domain_size();

        // build vectors of twiddles and inv_twiddles needed for FFT
        let root = field::get_root_of_unity(self.unextended_length() as u64);
        let inv_twiddles = fft::get_inv_twiddles(root, self.unextended_length());
        let root = field::get_root_of_unity(domain_size as u64);
        let twiddles = fft::get_twiddles(root, domain_size);

        // extend all registers
        for register in self.registers.iter_mut() {
            debug_assert!(register.capacity() == domain_size, "invalid capacity for register");
            // interpolate register trace into a polynomial
            polynom::interpolate_fft_twiddles(register, &inv_twiddles, true);

            // save the polynomial for later use
            self.polys.push(register.clone());

            // evaluate the polynomial over extended domain
            unsafe { register.set_len(register.capacity()); }
            polynom::eval_fft_twiddles(register, &twiddles, true);
        }
    }

    /// Puts the trace table into a Merkle tree such that each state of the table becomes
    /// a distinct leaf in the tree; all registers at a given step are hashed together to
    /// form a single leaf value.
    pub fn to_merkle_tree(&self, hash: HashFunction) -> MerkleTree {
        let mut trace_state = vec![0; self.register_count()];
        let mut hashed_states = to_quartic_vec(uninit_vector(self.domain_size() * 4));
        // TODO: this loop should be parallelized
        for i in 0..self.domain_size() {
            for j in 0..trace_state.len() {
                trace_state[j] = self.registers[j][i];
            }
            hash(&trace_state, &mut hashed_states[i]);
        }
        return MerkleTree::new(hashed_states, hash);
    }

    /// Evaluates trace polynomials at the specified point `z`; can be called only after
    /// the trace table has been extended
    pub fn eval_polys_at(&self, z: u64) -> Vec<u64> {
        assert!(self.is_extended(), "trace table has not been extended yet");

        let mut result = Vec::new();
        for poly in self.polys.iter() {
            result.push(polynom::eval(poly, z));
        }
        return result;
    }

    /// Combines trace polynomials for all registers into a single composition polynomial of the
    /// specified degree. The combination is done as follows:
    /// 1. First, state of trace registers at deep points z and z * g are computed;
    /// 2. Then, polynomials T1_i(x) = (T_i(x) - T_i(z)) / (x - z) and 
    /// T2_i(x) = (T_i(x) - T_i(z * g)) / (x - z * g) are computed for all i and combined
    /// together into a single polynomial using a pseudo-random linear combination;
    /// 3. Then the degree of the polynomial is adjusted to match the specified degree
    pub fn get_composition_poly(&self, z: u64, degree: usize, cc: &CompositionCoefficients) -> Vec<u64> {

        let trace_length = self.unextended_length();
        assert!(self.is_extended(), "trace table has not been extended yet");
        assert!(trace_length < degree, "composition degree must be greater than trace length");
        
        let g = field::get_root_of_unity(trace_length as u64);
        let next_z = field::mul(z, g);

        // compute state of registers at deep points z and z * g
        let deep_state1 = self.eval_polys_at(z);
        let deep_state2 = self.eval_polys_at(next_z);

        let mut t1_composition = vec![0; trace_length];
        let mut t2_composition = vec![0; trace_length];

        // combine trace polynomials into 2 composition polynomials T1(x) and T2(x)
        for i in 0..self.polys.len() {
            // compute T1(x) = (T(x) - T(z)), multiply it by a pseudo-random coefficient,
            // and add the result into composition polynomial
            parallel::mul_acc(&mut t1_composition, &self.polys[i], cc.trace1[i], 1);
            let adjusted_tz = field::mul(deep_state1[i], cc.trace1[i]);
            t1_composition[0] = field::sub(t1_composition[0], adjusted_tz);

            // compute T2(x) = (T(x) - T(z * g)), multiply it by a pseudo-random
            // coefficient, and add the result into composition polynomial
            parallel::mul_acc(&mut t2_composition, &self.polys[i], cc.trace2[i], 1);
            let adjusted_tz = field::mul(deep_state2[i], cc.trace2[i]);
            t2_composition[0] = field::sub(t2_composition[0], adjusted_tz);
        }

        // divide the two composition polynomials by (x - z) and (x - z * g)
        // respectively and add the resulting polynomials together
        polynom::syn_div_in_place(&mut t1_composition, field::neg(z));
        polynom::syn_div_in_place(&mut t2_composition, field::neg(next_z));
        parallel::add_in_place(&mut t1_composition, &t2_composition, 1);

        // adjust the degree of the polynomial to match the degree parameter by computing
        // C(x) = T(x) * k_1 + T(x) * x^incremental_degree * k_2
        let incremental_degree = degree - (trace_length - 2);
        let mut composition_poly = vec![0; degree.next_power_of_two()];
        // this is equivalent to T(x) * k_1
        parallel::mul_acc(
            &mut composition_poly[..trace_length],
            &t1_composition,
            cc.t1_degree,
            1);
        // this is equivalent to T(x) * x^incremental_degree * k_2
        parallel::mul_acc(
            &mut composition_poly[incremental_degree..(incremental_degree + trace_length)],
            &t1_composition,
            cc.t2_degree,
            1);
        
        return composition_poly;
    }
}

// TESTS
// ================================================================================================
#[cfg(test)]
mod tests {

    use crate::{ crypto::hash::blake3, processor::opcodes, utils::CopyInto };
    use crate::stark::{ TraceTable, CompositionCoefficients };
    use crate::math::{ field, polynom, parallel };

    const EXT_FACTOR: usize = 32;

    #[test]
    fn eval_polys_at() {
        let mut trace = build_trace_table();
        trace.extend();

        let g = field::get_root_of_unity(trace.unextended_length() as u64);

        let v1 = trace.eval_polys_at(g);
        let s1 = trace.get_state(1 * EXT_FACTOR);
        assert_eq!(v1, s1.registers());

        let v2 = trace.eval_polys_at(field::exp(g, 2));
        let s2 = trace.get_state(2 * EXT_FACTOR);
        assert_eq!(v2, s2.registers());
    }

    #[test]
    fn get_composition_poly() {

        let mut trace = build_trace_table();
        trace.extend();

        // compute trace composition polynomial
        let t_tree = trace.to_merkle_tree(blake3);
        let z = field::prng(t_tree.root().copy_into());
        let cc = CompositionCoefficients::new(t_tree.root());
        let target_degree = (trace.unextended_length() - 2) * 8 - 1;

        let g = field::get_root_of_unity(trace.unextended_length() as u64);
        let zg = field::mul(z, g);

        let composition_poly = trace.get_composition_poly(z, target_degree, &cc);
        let mut actual_evaluations = composition_poly.clone();
        polynom::eval_fft(&mut actual_evaluations, true);
        assert_eq!(target_degree, polynom::infer_degree(&actual_evaluations));

        // compute expected evaluations
        let domain_size = target_degree.next_power_of_two();
        let domain_root = field::get_root_of_unity(domain_size as u64);
        let domain = field::get_power_series(domain_root, domain_size);

        let mut expected_evaluations = vec![0; domain_size];

        let tz = trace.eval_polys_at(z);
        let tzg = trace.eval_polys_at(zg);

        for i in 0..trace.register_count() {
            // add T1(x) to expected evaluations
            let mut trace_poly = trace.get_register_poly(i).to_vec();
            trace_poly.resize(domain_size, 0);
            polynom::eval_fft(&mut trace_poly, true);
            parallel::sub_const_in_place(&mut trace_poly, tz[i], 1);
            for j in 0..trace_poly.len() {
                trace_poly[j] = field::div(trace_poly[j], field::sub(domain[j], z));
            }
            parallel::mul_acc(&mut expected_evaluations, &trace_poly, cc.trace1[i], 1);

            // add T2(x) to expected evaluations
            let mut trace_poly = trace.get_register_poly(i).to_vec();
            trace_poly.resize(domain_size, 0);
            polynom::eval_fft(&mut trace_poly, true);
            parallel::sub_const_in_place(&mut trace_poly, tzg[i], 1);
            for j in 0..trace_poly.len() {
                trace_poly[j] = field::div(trace_poly[j], field::sub(domain[j], zg));
            }
            parallel::mul_acc(&mut expected_evaluations, &trace_poly, cc.trace2[i], 1);
        }

        // raise degree
        let incremental_degree = target_degree - (trace.unextended_length() - 2);
        for i in 0..domain.len() {
            let y = expected_evaluations[i];
            let y1 = field::mul(y, cc.t1_degree);

            let xp = field::exp(domain[i], incremental_degree as u64);
            let y2 = field::mul(field::mul(y, xp), cc.t2_degree);
            expected_evaluations[i] = field::add(y1, y2);
        }

        assert_eq!(expected_evaluations, actual_evaluations);
    }

    fn build_trace_table() -> TraceTable {
        let program = [
            opcodes::DUP0, opcodes::PULL2, opcodes::ADD,
            opcodes::DUP0, opcodes::PULL2, opcodes::ADD,
            opcodes::DUP0, opcodes::PULL2, opcodes::ADD,
            opcodes::DUP0, opcodes::PULL2, opcodes::ADD,
            opcodes::DUP0, opcodes::PULL2, opcodes::ADD,
            opcodes::NOOP
        ];
        return TraceTable::new(&program, &[1, 0], EXT_FACTOR);
    }
}