use statrs::distribution::Normal;

pub mod distributions;

use distributions::{Categorical, DiscreteDistribution, LeakyQuantizer};

const FREQUENCY_BITS: usize = 24;

pub struct AnsCoder {
    buf: Vec<u32>,
    state: u64,
}

impl AnsCoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            state: 1 << 32,
        }
    }

    pub fn push_symbol<S: Copy>(&mut self, symbol: S, distribution: &impl DiscreteDistribution<S>) {
        let (left_sided_cumulative, probability) =
            distribution.left_cumulative_and_probability(symbol);
        if self.state >= (probability as u64) << (64 - FREQUENCY_BITS) {
            self.buf.push(self.state as u32);
            self.state >>= 32;
        }
        let prefix = self.state / probability as u64;
        let suffix = self.state % probability as u64 + left_sided_cumulative as u64;
        self.state = (prefix << FREQUENCY_BITS) | suffix;
    }

    pub fn pop_symbol<S: Copy>(
        &mut self,
        distribution: &impl DiscreteDistribution<S>,
    ) -> Result<S, ()> {
        let prefix = self.state >> FREQUENCY_BITS;
        let suffix = (self.state % (1 << FREQUENCY_BITS)) as u32;
        let (symbol, left_sided_cumulative, probability) = distribution.quantile_function(suffix);
        self.state = probability as u64 * prefix + (suffix - left_sided_cumulative) as u64;

        if self.state < (1 << 32) {
            let word = self.buf.pop().ok_or(())?;
            self.state = (self.state << 32) | word as u64;
        }

        Ok(symbol)
    }

    pub fn finish_encoding(mut self) -> Vec<u32> {
        self.buf.push(self.state as u32);
        self.buf.push((self.state >> 32) as u32);
        self.buf
    }

    pub fn finish_decoding(self) -> Result<(), ()> {
        if self.buf.is_empty() && self.state == 1 << 32 {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Returns the number of bits that would be serialized by [`finish_encoding`]
    ///
    /// This includes the constant overhead of encoding. The returned value is at
    /// least 64 and a multiple of 32.;
    ///
    /// [`finish_encoding`]: #method.finish_encoding
    pub fn num_bits(&self) -> usize {
        32 * self.buf.len() + 64
    }

    pub fn push_gaussian_symbols(&mut self, symbols: &[i32], means: &[f64], stds: &[f64]) {
        assert_eq!(symbols.len(), means.len());
        assert_eq!(symbols.len(), stds.len());

        let quantizer = LeakyQuantizer::new(1 - (1 << 15), 1 << 15);
        for ((&symbol, &mean), &std) in symbols.iter().zip(means).zip(stds) {
            let distribution = quantizer.quantize(Normal::new(mean, std).unwrap());
            self.push_symbol(symbol, &distribution)
        }
    }

    pub fn pop_gaussian_symbols(&mut self, means: &[f64], stds: &[f64]) -> Result<Vec<i32>, ()> {
        assert_eq!(means.len(), stds.len());
        if means.is_empty() {
            return Ok(Vec::new());
        }

        let quantizer = LeakyQuantizer::new(1 - (1 << 15), 1 << 15);
        let mut symbols = Vec::<i32>::with_capacity(means.len());

        unsafe {
            // SAFETY: we know that `symbols` is not empty because of the check at the
            // beginning of the method, so `symbols.as_mut_ptr()` points to a valid
            // location. Also, `i32` has no destructor, so it's OK to return early from
            // this method in case `pop_symbol` returns an error.
            let symbols_slice = std::slice::from_raw_parts_mut(symbols.as_mut_ptr(), means.len());

            for ((symbol, &mean), &std) in symbols_slice.iter_mut().zip(means).zip(stds).rev() {
                let distribution = quantizer.quantize(Normal::new(mean, std).unwrap());
                *symbol = self.pop_symbol(&distribution)?;
            }

            symbols.set_len(means.len());
        }

        Ok(symbols)
    }

    pub fn push_iid_categorical_symbols(
        &mut self,
        symbols: &[i32],
        min_supported_symbol: i32,
        max_supported_symbol: i32,
        min_provided_symbol: i32,
        probabilities: &[f64],
    ) {
        let distribution = Categorical::from_continuous_probabilities(
            min_supported_symbol,
            max_supported_symbol,
            min_provided_symbol,
            probabilities,
        );

        for &symbol in symbols {
            self.push_symbol(symbol, &distribution);
        }
    }

    pub fn pop_iid_categorical_symbols(
        &mut self,
        amt: usize,
        min_supported_symbol: i32,
        max_supported_symbol: i32,
        min_provided_symbol: i32,
        probabilities: &[f64],
    ) -> Result<Vec<i32>, ()> {
        if amt == 0 {
            return Ok(Vec::new());
        }

        let distribution = Categorical::from_continuous_probabilities(
            min_supported_symbol,
            max_supported_symbol,
            min_provided_symbol,
            probabilities,
        );

        let mut symbols = Vec::<i32>::with_capacity(amt);
        unsafe {
            // SAFETY: we know that `symbols` is not empty because of the check at the
            // beginning of the method, so `symbols.as_mut_ptr()` points to a valid
            // location. Also, `i32` has no destructor, so it's OK to return early from
            // this method in case `pop_symbol` returns an error.
            let symbols_slice = std::slice::from_raw_parts_mut(symbols.as_mut_ptr(), amt);

            for symbol in symbols_slice.iter_mut().rev() {
                *symbol = self.pop_symbol(&distribution)?;
            }

            symbols.set_len(amt);
        }

        Ok(symbols)
    }
}

#[cfg(test)]
mod tests {
    use super::distributions::DiscreteDistribution;
    use super::*;

    use rand_xoshiro::rand_core::{RngCore, SeedableRng};
    use rand_xoshiro::Xoshiro256StarStar;
    use statrs::distribution::{InverseCDF, Normal};

    #[test]
    fn compress_few() {
        let mut coder = AnsCoder::new();
        let quantizer = LeakyQuantizer::new(-127, 127);
        let distribution = quantizer.quantize(Normal::new(3.2, 5.1).unwrap());

        coder.push_symbol(3, &distribution);
        coder.push_symbol(100, &distribution);

        assert_eq!(coder.pop_symbol(&distribution).unwrap(), 100);
        assert_eq!(coder.pop_symbol(&distribution).unwrap(), 3);

        coder.finish_decoding().unwrap();
    }

    #[test]
    fn compress_many() {
        const AMT: usize = 1000;
        let mut symbols_gaussian = Vec::with_capacity(AMT);
        let mut means = Vec::with_capacity(AMT);
        let mut stds = Vec::with_capacity(AMT);

        let mut rng = Xoshiro256StarStar::seed_from_u64(1234);
        for _ in 0..AMT {
            let mean = (200.0 / std::u32::MAX as f64) * rng.next_u32() as f64 - 100.0;
            let std_dev = (10.0 / std::u32::MAX as f64) * rng.next_u32() as f64 + 0.001;
            let quantile = (rng.next_u32() as f64 + 0.5) / (1u64 << 32) as f64;
            let dist = Normal::new(mean, std_dev).unwrap();
            let symbol = std::cmp::min(
                -127,
                std::cmp::max(127, (dist.inverse_cdf(quantile) + 0.5) as i32),
            );

            symbols_gaussian.push(symbol);
            means.push(mean);
            stds.push(std_dev);
        }

        let hist = [
            1u32, 186545, 237403, 295700, 361445, 433686, 509456, 586943, 663946, 737772, 1657269,
            896675, 922197, 930672, 916665, 0, 0, 0, 0, 0, 723031, 650522, 572300, 494702, 418703,
            347600, 1, 283500, 226158, 178194, 136301, 103158, 76823, 55540, 39258, 27988, 54269,
        ];
        let categorical_probabilities = hist.iter().map(|&x| x as f64).collect::<Vec<_>>();
        let categorical =
            Categorical::from_continuous_probabilities(-127, 127, -10, &categorical_probabilities);
        let mut symbols_categorical = Vec::with_capacity(AMT);
        for _ in 0..AMT {
            let quantile = rng.next_u32() & ((1u64 << super::FREQUENCY_BITS) - 1) as u32;
            let symbol = categorical.quantile_function(quantile).0;
            symbols_categorical.push(symbol);
        }

        let mut coder = AnsCoder::new();

        coder.push_iid_categorical_symbols(
            &symbols_categorical,
            -127,
            127,
            -10,
            &categorical_probabilities,
        );
        dbg!(coder.num_bits(), AMT as f64 * categorical.entropy());

        coder.push_gaussian_symbols(&symbols_gaussian, &means, &stds);

        let reconstructed_gaussian = coder.pop_gaussian_symbols(&means, &stds).unwrap();
        let reconstructed_categorical = coder
            .pop_iid_categorical_symbols(AMT, -127, 127, -10, &categorical_probabilities)
            .unwrap();

        coder.finish_decoding().unwrap();

        assert_eq!(symbols_gaussian, reconstructed_gaussian);
        assert_eq!(symbols_categorical, reconstructed_categorical);
    }
}
