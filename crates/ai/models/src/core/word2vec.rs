//===----------------------------------------------------------------------===//
//                         GFS
//
// word2vec.rs
//
// Word2Vec implementation for training word embeddings
// Migrated from DuckDB's extension/semantic_search/core/word2vec.cpp
//===----------------------------------------------------------------------===//

use super::vocab::Vocabulary;
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

/// Word2Vec architecture type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum W2VArchitecture {
    /// Skip-gram: predict context from center word
    SkipGram,
    /// CBOW (Continuous Bag of Words): predict center word from context
    CBOW,
}

/// Configuration for Word2Vec training
#[derive(Debug, Clone)]
pub struct Word2VecConfig {
    /// Embedding vector dimension
    pub dimension: usize,
    /// Context window size (words on each side)
    pub window: usize,
    /// Number of negative samples per positive example
    pub negative_samples: usize,
    /// Minimum word frequency threshold
    pub min_count: u64,
    /// Number of training epochs
    pub epochs: usize,
    /// Number of threads for parallel training
    pub num_threads: usize,
    /// Initial learning rate
    pub learning_rate: f64,
    /// Minimum learning rate (learning rate decays during training)
    pub min_learning_rate: f64,
    /// Subsampling threshold for frequent words
    pub subsample_threshold: f64,
    /// Architecture (Skip-gram or CBOW)
    pub arch: W2VArchitecture,
}

impl Default for Word2VecConfig {
    fn default() -> Self {
        Self {
            dimension: 100,
            window: 5,
            negative_samples: 5,
            min_count: 5,
            epochs: 5,
            num_threads: 4,
            learning_rate: 0.025,
            min_learning_rate: 0.0001,
            subsample_threshold: 1e-5,
            arch: W2VArchitecture::SkipGram,
        }
    }
}

/// Fast random number generator using Xorshift128
///
/// This is faster than the standard library RNG for our use case
struct Xorshift128 {
    s: [u64; 2],
}

impl Xorshift128 {
    /// Create a new RNG with given seed
    fn new(seed: u64) -> Self {
        Self {
            s: [
                seed,
                seed.wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407),
            ],
        }
    }

    /// Generate next random u64
    fn next(&mut self) -> u64 {
        let s1 = self.s[0];
        let s0 = self.s[1];
        self.s[0] = s0;
        let s1 = s1 ^ (s1 << 23);
        self.s[1] = s1 ^ s0 ^ (s1 >> 17) ^ (s0 >> 26);
        self.s[1].wrapping_add(s0)
    }

    /// Generate random f64 in [0, 1)
    fn next_double(&mut self) -> f64 {
        (self.next() >> 11) as f64 * (1.0 / 9007199254740992.0)
    }
}

/// Wrapper for raw pointer that implements Send and Copy
/// SAFETY: We guarantee that the pointer usage is thread-safe
#[derive(Copy, Clone)]
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// Word2Vec model for training and generating word embeddings
pub struct Word2Vec {
    /// Configuration
    cfg: Word2VecConfig,
    /// Vocabulary
    vocab: Vocabulary,
    /// Input embeddings (V × D matrix, stored as flat vector)
    w_in: Vec<f32>,
    /// Output embeddings (V × D matrix, for negative sampling)
    w_out: Vec<f32>,
    /// Atomic counter for words processed (for learning rate decay)
    words_processed: Arc<AtomicU64>,
}

impl Word2Vec {
    /// Create a new Word2Vec model with default configuration
    pub fn new() -> Self {
        Self::with_config(Word2VecConfig::default())
    }

    /// Create a new Word2Vec model with custom configuration
    pub fn with_config(cfg: Word2VecConfig) -> Self {
        Self {
            cfg,
            vocab: Vocabulary::new(),
            w_in: Vec::new(),
            w_out: Vec::new(),
            words_processed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the embedding dimension
    pub fn dimension(&self) -> usize {
        self.cfg.dimension
    }

    /// Get reference to the vocabulary
    pub fn vocab(&self) -> &Vocabulary {
        &self.vocab
    }

    /// Train the model on a corpus
    ///
    /// # Arguments
    /// * `corpus` - Vector of text documents to train on
    pub fn train(&mut self, corpus: &[String]) {
        // Build vocabulary
        self.vocab.build(corpus, self.cfg.min_count);
        let vocab_size = self.vocab.size();

        if vocab_size == 0 {
            return;
        }

        // Initialize embedding matrices
        let total_size = vocab_size * self.cfg.dimension;
        self.w_in.resize(total_size, 0.0);
        self.w_out.resize(total_size, 0.0);

        // Initialize input embeddings with random values
        let mut rng = rand::thread_rng();
        use rand::Rng;
        let init_range = 0.5 / (self.cfg.dimension as f32);
        for val in &mut self.w_in {
            *val = rng.gen_range(-init_range..init_range);
        }

        // Reset word counter
        self.words_processed.store(0, Ordering::SeqCst);
        let total_train = (self.cfg.epochs as u64) * self.vocab.train_words();

        // Wrap raw pointers in Send wrapper
        let w_in_ptr = SendPtr(self.w_in.as_mut_ptr());
        let w_out_ptr = SendPtr(self.w_out.as_mut_ptr());

        // Use scoped threads to allow borrowing
        thread::scope(|s| {
            for thread_id in 0..self.cfg.num_threads {
                let cfg = &self.cfg;
                let vocab = &self.vocab;
                let words_processed = &self.words_processed;
                let w_in = w_in_ptr;
                let w_out = w_out_ptr;

                s.spawn(move || {
                    Self::train_thread(
                        corpus,
                        thread_id,
                        total_train,
                        cfg,
                        vocab,
                        w_in,
                        w_out,
                        words_processed,
                    );
                });
            }
        });
    }

    /// Training thread function
    ///
    /// SAFETY: This function uses unsafe pointer access to embeddings.
    /// It's safe because each thread works on non-overlapping documents,
    /// and updates to shared embeddings are done atomically via gradient updates.
    #[allow(clippy::too_many_arguments)]
    fn train_thread(
        corpus: &[String],
        thread_id: usize,
        total_train: u64,
        cfg: &Word2VecConfig,
        vocab: &Vocabulary,
        w_in_ptr: SendPtr<f32>,
        w_out_ptr: SendPtr<f32>,
        words_processed: &AtomicU64,
    ) {
        // Extract raw pointers from SendPtr
        let w_in_ptr = w_in_ptr.0;
        let w_out_ptr = w_out_ptr.0;
        let mut rng = Xorshift128::new((thread_id + 1) as u64);
        let mut grad_buf = vec![0.0f32; cfg.dimension];
        let mut hidden = vec![0.0f32; cfg.dimension];

        // Calculate chunk for this thread
        let chunk_size = corpus.len() / cfg.num_threads;
        let start = thread_id * chunk_size;
        let end = if thread_id == cfg.num_threads - 1 {
            corpus.len()
        } else {
            start + chunk_size
        };

        for _epoch in 0..cfg.epochs {
            for doc in &corpus[start..end] {
                let indices = vocab.doc_to_indices(doc);

                // Apply subsampling
                let mut sampled = Vec::with_capacity(indices.len());
                for &idx in &indices {
                    if rng.next_double() >= vocab.discard_probability(idx) {
                        sampled.push(idx);
                    }
                }

                // Train on each word in the document
                for i in 0..sampled.len() {
                    // Update word counter and learning rate
                    let processed = words_processed.fetch_add(1, Ordering::Relaxed);
                    let progress = (processed as f64) / (total_train as f64);
                    let mut lr = cfg.learning_rate * (1.0 - progress);
                    if lr < cfg.min_learning_rate {
                        lr = cfg.min_learning_rate;
                    }
                    let lr = lr as f32;

                    // Dynamic window size
                    let dynamic_window = 1 + (rng.next() as usize % cfg.window);

                    // SAFETY: We're accessing non-overlapping parts of the embedding matrices
                    // Each thread works on different documents/words
                    unsafe {
                        if cfg.arch == W2VArchitecture::SkipGram {
                            // Skip-gram: predict context from center word
                            for j in -(dynamic_window as isize)..=(dynamic_window as isize) {
                                if j == 0 {
                                    continue;
                                }
                                let ctx_pos = (i as isize) + j;
                                if ctx_pos < 0 || ctx_pos >= sampled.len() as isize {
                                    continue;
                                }
                                Self::train_skipgram_pair(
                                    sampled[i],
                                    sampled[ctx_pos as usize],
                                    lr,
                                    &mut rng,
                                    &mut grad_buf,
                                    cfg,
                                    vocab,
                                    w_in_ptr,
                                    w_out_ptr,
                                );
                            }
                        } else {
                            // CBOW: predict center word from context
                            let mut ctx = Vec::new();
                            for j in -(dynamic_window as isize)..=(dynamic_window as isize) {
                                if j == 0 {
                                    continue;
                                }
                                let ctx_pos = (i as isize) + j;
                                if ctx_pos >= 0 && ctx_pos < sampled.len() as isize {
                                    ctx.push(sampled[ctx_pos as usize]);
                                }
                            }
                            Self::train_cbow_window(
                                &ctx,
                                sampled[i],
                                lr,
                                &mut rng,
                                &mut hidden,
                                &mut grad_buf,
                                cfg,
                                vocab,
                                w_in_ptr,
                                w_out_ptr,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Fast sigmoid approximation
    #[inline]
    fn sigmoid(x: f32) -> f32 {
        if x > 6.0 {
            return 1.0;
        }
        if x < -6.0 {
            return 0.0;
        }
        1.0 / (1.0 + (-x).exp())
    }

    /// Train a single skip-gram pair (center word, context word)
    #[allow(clippy::too_many_arguments)]
    unsafe fn train_skipgram_pair(
        center: usize,
        context: usize,
        lr: f32,
        rng: &mut Xorshift128,
        grad_buf: &mut [f32],
        cfg: &Word2VecConfig,
        vocab: &Vocabulary,
        w_in_ptr: *mut f32,
        w_out_ptr: *mut f32,
    ) {
        let dim = cfg.dimension;
        let v_in = std::slice::from_raw_parts_mut(
            w_in_ptr.add(center * dim),
            dim,
        );

        grad_buf.fill(0.0);

        // Positive sample
        {
            let v_out = std::slice::from_raw_parts_mut(
                w_out_ptr.add(context * dim),
                dim,
            );

            let dot: f32 = (0..dim).map(|d| v_in[d] * v_out[d]).sum();
            let g = (1.0 - Self::sigmoid(dot)) * lr;

            for d in 0..dim {
                grad_buf[d] += g * v_out[d];
                v_out[d] += g * v_in[d];
            }
        }

        // Negative samples
        for _ in 0..cfg.negative_samples {
            let neg = vocab.sample_negative(rng.next());
            if neg == context {
                continue;
            }

            let v_out = std::slice::from_raw_parts_mut(
                w_out_ptr.add(neg * dim),
                dim,
            );

            let dot: f32 = (0..dim).map(|d| v_in[d] * v_out[d]).sum();
            let g = -Self::sigmoid(dot) * lr;

            for d in 0..dim {
                grad_buf[d] += g * v_out[d];
                v_out[d] += g * v_in[d];
            }
        }

        // Update input embedding
        for d in 0..dim {
            v_in[d] += grad_buf[d];
        }
    }

    /// Train CBOW window (predict target from context words)
    #[allow(clippy::too_many_arguments)]
    unsafe fn train_cbow_window(
        context_words: &[usize],
        target: usize,
        lr: f32,
        rng: &mut Xorshift128,
        hidden: &mut [f32],
        grad_buf: &mut [f32],
        cfg: &Word2VecConfig,
        vocab: &Vocabulary,
        w_in_ptr: *mut f32,
        w_out_ptr: *mut f32,
    ) {
        let dim = cfg.dimension;

        if context_words.is_empty() {
            return;
        }

        // Compute hidden layer (average of context word embeddings)
        hidden.fill(0.0);
        for &cw in context_words {
            let v = std::slice::from_raw_parts(
                w_in_ptr.add(cw * dim),
                dim,
            );
            for d in 0..dim {
                hidden[d] += v[d];
            }
        }

        let inv = 1.0 / (context_words.len() as f32);
        for val in hidden.iter_mut() {
            *val *= inv;
        }

        grad_buf.fill(0.0);

        // Positive sample
        {
            let v_out = std::slice::from_raw_parts_mut(
                w_out_ptr.add(target * dim),
                dim,
            );

            let dot: f32 = (0..dim).map(|d| hidden[d] * v_out[d]).sum();
            let g = (1.0 - Self::sigmoid(dot)) * lr;

            for d in 0..dim {
                grad_buf[d] += g * v_out[d];
                v_out[d] += g * hidden[d];
            }
        }

        // Negative samples
        for _ in 0..cfg.negative_samples {
            let neg = vocab.sample_negative(rng.next());
            if neg == target {
                continue;
            }

            let v_out = std::slice::from_raw_parts_mut(
                w_out_ptr.add(neg * dim),
                dim,
            );

            let dot: f32 = (0..dim).map(|d| hidden[d] * v_out[d]).sum();
            let g = -Self::sigmoid(dot) * lr;

            for d in 0..dim {
                grad_buf[d] += g * v_out[d];
                v_out[d] += g * hidden[d];
            }
        }

        // Update context word embeddings
        for &cw in context_words {
            let v_in = std::slice::from_raw_parts_mut(
                w_in_ptr.add(cw * dim),
                dim,
            );
            for d in 0..dim {
                v_in[d] += grad_buf[d] * inv;
            }
        }
    }

    /// Get the embedding vector for a word by index
    ///
    /// # Arguments
    /// * `word_idx` - Word index in vocabulary
    ///
    /// # Returns
    /// Some(&[f32]) if word exists, None otherwise
    pub fn embedding(&self, word_idx: usize) -> Option<&[f32]> {
        if word_idx >= self.vocab.size() || self.w_in.is_empty() {
            return None;
        }

        let start = word_idx * self.cfg.dimension;
        let end = start + self.cfg.dimension;

        if end > self.w_in.len() {
            return None;
        }

        Some(&self.w_in[start..end])
    }

    /// Generate embedding for text (average of word embeddings, normalized)
    ///
    /// # Arguments
    /// * `text` - Input text
    ///
    /// # Returns
    /// Normalized embedding vector
    pub fn embed_text(&self, text: &str) -> Vec<f32> {
        let indices = self.vocab.doc_to_indices(text);
        let mut result = vec![0.0f32; self.cfg.dimension];

        if indices.is_empty() {
            return result;
        }

        // Average word embeddings
        for &idx in &indices {
            if let Some(vec) = self.embedding(idx) {
                for (i, &val) in vec.iter().enumerate() {
                    result[i] += val;
                }
            }
        }

        let inv_n = 1.0 / (indices.len() as f32);
        for val in &mut result {
            *val *= inv_n;
        }

        // Normalize
        let norm: f32 = result.iter().map(|&v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for val in &mut result {
                *val /= norm;
            }
        }

        result
    }

    /// Save model to file
    ///
    /// # Arguments
    /// * `path` - File path to save to
    pub fn save(&self, path: &str) -> io::Result<()> {
        let mut file = File::create(path)?;

        // Write config
        file.write_u32::<LittleEndian>(self.cfg.dimension as u32)?;
        file.write_u32::<LittleEndian>(self.cfg.window as u32)?;
        file.write_u32::<LittleEndian>(self.cfg.negative_samples as u32)?;
        file.write_u32::<LittleEndian>(self.cfg.min_count as u32)?;
        file.write_u32::<LittleEndian>(self.cfg.epochs as u32)?;
        file.write_u32::<LittleEndian>(self.cfg.num_threads as u32)?;
        file.write_f64::<LittleEndian>(self.cfg.learning_rate)?;
        file.write_f64::<LittleEndian>(self.cfg.min_learning_rate)?;
        file.write_f64::<LittleEndian>(self.cfg.subsample_threshold)?;
        file.write_u32::<LittleEndian>(self.cfg.arch as u32)?;

        // Write vocabulary
        let vocab_size = self.vocab.size();
        file.write_u32::<LittleEndian>(vocab_size as u32)?;

        for i in 0..vocab_size {
            let word = self.vocab.word_at(i);
            let count = self.vocab.count_at(i);

            file.write_u32::<LittleEndian>(word.len() as u32)?;
            file.write_all(word.as_bytes())?;
            file.write_u64::<LittleEndian>(count)?;
        }

        // Write embeddings
        for &val in &self.w_in {
            file.write_f32::<LittleEndian>(val)?;
        }

        Ok(())
    }

    /// Load model from file
    ///
    /// # Arguments
    /// * `path` - File path to load from
    pub fn load(&mut self, path: &str) -> io::Result<()> {
        let mut file = File::open(path)?;

        // Read config
        self.cfg.dimension = file.read_u32::<LittleEndian>()? as usize;
        self.cfg.window = file.read_u32::<LittleEndian>()? as usize;
        self.cfg.negative_samples = file.read_u32::<LittleEndian>()? as usize;
        self.cfg.min_count = file.read_u32::<LittleEndian>()? as u64;
        self.cfg.epochs = file.read_u32::<LittleEndian>()? as usize;
        self.cfg.num_threads = file.read_u32::<LittleEndian>()? as usize;
        self.cfg.learning_rate = file.read_f64::<LittleEndian>()?;
        self.cfg.min_learning_rate = file.read_f64::<LittleEndian>()?;
        self.cfg.subsample_threshold = file.read_f64::<LittleEndian>()?;
        let arch_int = file.read_u32::<LittleEndian>()?;
        self.cfg.arch = if arch_int == 0 {
            W2VArchitecture::SkipGram
        } else {
            W2VArchitecture::CBOW
        };

        // Read vocabulary
        let vocab_size = file.read_u32::<LittleEndian>()? as usize;
        let mut words = Vec::with_capacity(vocab_size);
        let mut counts = Vec::with_capacity(vocab_size);

        for _ in 0..vocab_size {
            let word_len = file.read_u32::<LittleEndian>()? as usize;
            let mut word_bytes = vec![0u8; word_len];
            file.read_exact(&mut word_bytes)?;
            let word = String::from_utf8(word_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let count = file.read_u64::<LittleEndian>()?;

            words.push(word);
            counts.push(count);
        }

        // Restore vocabulary
        self.vocab = Vocabulary::new();
        self.vocab.restore(&words, &counts);

        // Read embeddings
        self.w_in.clear();
        self.w_in.reserve(vocab_size * self.cfg.dimension);
        for _ in 0..(vocab_size * self.cfg.dimension) {
            self.w_in.push(file.read_f32::<LittleEndian>()?);
        }

        // Initialize w_out (not used for inference, but must be allocated)
        self.w_out.clear();
        self.w_out.resize(vocab_size * self.cfg.dimension, 0.0);

        Ok(())
    }
}

impl Default for Word2Vec {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xorshift128() {
        let mut rng = Xorshift128::new(42);
        let val1 = rng.next();
        let val2 = rng.next();
        assert_ne!(val1, val2);

        let d = rng.next_double();
        assert!(d >= 0.0 && d < 1.0);
    }

    #[test]
    fn test_sigmoid() {
        assert_eq!(Word2Vec::sigmoid(100.0), 1.0);
        assert_eq!(Word2Vec::sigmoid(-100.0), 0.0);
        assert!((Word2Vec::sigmoid(0.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_word2vec_config_default() {
        let cfg = Word2VecConfig::default();
        assert_eq!(cfg.dimension, 100);
        assert_eq!(cfg.window, 5);
        assert_eq!(cfg.arch, W2VArchitecture::SkipGram);
    }

    #[test]
    fn test_word2vec_new() {
        let model = Word2Vec::new();
        assert_eq!(model.dimension(), 100);
        assert_eq!(model.vocab().size(), 0);
    }

    #[test]
    fn test_word2vec_train_small() {
        let corpus = vec![
            "hello world from rust".to_string(),
            "machine learning with word2vec".to_string(),
            "hello machine learning".to_string(),
        ];

        let mut cfg = Word2VecConfig::default();
        cfg.dimension = 10;
        cfg.epochs = 1;
        cfg.min_count = 1;
        cfg.num_threads = 1;

        let mut model = Word2Vec::with_config(cfg);
        model.train(&corpus);

        assert!(model.vocab().size() > 0);
        assert!(!model.w_in.is_empty());
    }

    #[test]
    fn test_word2vec_embed_text() {
        let corpus = vec![
            "apple banana cherry".to_string(),
            "apple banana date".to_string(),
            "apple elderberry fig".to_string(),
        ];

        let mut cfg = Word2VecConfig::default();
        cfg.dimension = 10;
        cfg.epochs = 2;
        cfg.min_count = 1;
        cfg.num_threads = 1;

        let mut model = Word2Vec::with_config(cfg);
        model.train(&corpus);

        let embedding = model.embed_text("apple banana");
        assert_eq!(embedding.len(), 10);

        // Check normalization
        let norm: f32 = embedding.iter().map(|&v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01 || norm == 0.0);
    }

    #[test]
    fn test_word2vec_save_load() {
        let corpus = vec![
            "test save load functionality".to_string(),
        ];

        let mut cfg = Word2VecConfig::default();
        cfg.dimension = 5;
        cfg.epochs = 1;
        cfg.min_count = 1;

        let mut model1 = Word2Vec::with_config(cfg);
        model1.train(&corpus);

        // Save
        let path = "/tmp/test_word2vec.bin";
        model1.save(path).unwrap();

        // Load
        let mut model2 = Word2Vec::new();
        model2.load(path).unwrap();

        assert_eq!(model2.dimension(), 5);
        assert_eq!(model2.vocab().size(), model1.vocab().size());

        // Clean up
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn test_architecture_cbow() {
        let corpus = vec![
            "hello world".to_string(),
            "hello rust".to_string(),
        ];

        let mut cfg = Word2VecConfig::default();
        cfg.dimension = 10;
        cfg.epochs = 1;
        cfg.min_count = 1;
        cfg.num_threads = 1;
        cfg.arch = W2VArchitecture::CBOW;

        let mut model = Word2Vec::with_config(cfg);
        model.train(&corpus);

        assert!(model.vocab().size() > 0);
    }
}
