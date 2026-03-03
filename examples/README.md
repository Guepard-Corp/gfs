# GFS Training Configuration Examples

This directory contains example training configurations for GNN models.

## Files

- **`train_config.toml`** - Full production-ready configuration with all options documented
- **`train_config_fast.toml`** - Quick training for development and testing
- **`train_config_quality.toml`** - High-quality embeddings for production use

---

## Using These Configurations

### Basic Usage

```bash
# Train with the default configuration
gfs train start --output models/my_model --config examples/train_config.toml
```

### Override Configuration

You can override specific settings via command-line arguments:

```bash
# Override tables
gfs train start --output models/my_model --config examples/train_config.toml \
  --tables products,categories,orders

# Override row limit
gfs train start --output models/my_model --config examples/train_config.toml \
  --row-limit 5000

# Skip metrics for faster training
gfs train start --output models/my_model --config examples/train_config.toml \
  --skip-metrics
```

---

## Configuration Profiles

### 1. Default Configuration (`train_config.toml`)

**Best for**: General-purpose training with good balance of speed and quality

**Characteristics**:
- 128-dimensional embeddings
- 10 training epochs
- Evaluation metrics enabled
- Checkpointing every 2 epochs

**Training time**: ~30-60 seconds per 10k rows

**Use when**:
- Starting with a new dataset
- Need good quality without long training
- Want comprehensive metrics

**Command**:
```bash
gfs train start --output models/balanced_model --config examples/train_config.toml
```

---

### 2. Fast Training (`train_config_fast.toml`)

**Best for**: Development, testing, rapid iteration

**Characteristics**:
- 64-dimensional embeddings (2x faster)
- 5 training epochs
- Fewer random walks
- Metrics disabled
- No checkpointing

**Training time**: ~10-20 seconds per 10k rows

**Use when**:
- Developing and testing pipelines
- Exploring data structure
- Quick quality checks
- Hyperparameter search

**Command**:
```bash
gfs train start --output models/dev_model --config examples/train_config_fast.toml
```

---

### 3. Quality Training (`train_config_quality.toml`)

**Best for**: Production models, final training runs

**Characteristics**:
- 256-dimensional embeddings
- 20 training epochs
- More random walks
- Lower learning rate
- Better HNSW index

**Training time**: ~2-4 minutes per 10k rows

**Use when**:
- Training final production models
- Need highest quality embeddings
- Have time for thorough training
- Semantic search quality is critical

**Command**:
```bash
gfs train start --output models/production_model --config examples/train_config_quality.toml
```

---

## Configuration Reference

### Model Parameters

#### Word2Vec Settings (`[model.w2v]`)

```toml
dimension = 128              # Embedding size (64, 128, 256, or 512)
                            # Higher = more expressive, slower

epochs = 10                  # Training iterations
                            # More epochs = better convergence, longer training

learning_rate = 0.025        # Training step size
                            # Lower = more stable, slower

window = 5                   # Context window size
                            # Larger = more global context

negative_samples = 15        # Negative sampling count
                            # More = better quality, slower

min_count = 1                # Minimum word frequency
                            # Higher = smaller vocabulary, faster

num_threads = 8              # Parallel threads
                            # Set to CPU core count
```

**Recommendations**:
- **Small datasets (<10k rows)**: `dimension=64, epochs=5`
- **Medium datasets (10k-100k)**: `dimension=128, epochs=10`
- **Large datasets (>100k)**: `dimension=256, epochs=20`

#### HNSW Index (`[model.hnsw]`)

```toml
m = 16                       # Number of connections per node
                            # Higher = better recall, more memory

ef_construction = 200        # Build-time search depth
                            # Higher = better index quality, slower build

ef = 200                     # Query-time search depth
                            # Higher = better recall, slower search
```

**Recommendations**:
- **Fast search, moderate quality**: `m=8, ef_construction=100, ef=100`
- **Balanced**: `m=16, ef_construction=200, ef=200`
- **High quality, slower**: `m=32, ef_construction=400, ef=400`

#### Random Walk (`[model.walker]`)

```toml
num_walks = 10               # Walks per node
                            # More = better coverage, longer training

walk_length = 20             # Steps per walk
                            # Longer = more global structure

seed = 42                    # Random seed for reproducibility
```

**Recommendations**:
- **Fast**: `num_walks=5, walk_length=10`
- **Balanced**: `num_walks=10, walk_length=20`
- **Thorough**: `num_walks=20, walk_length=40`

### Data Settings (`[data]`)

```toml
table_filter = ["products", "orders"]  # Specific tables to include
                                       # Omit for all tables

row_limit = 10000                      # Max rows per table
                                       # Omit for all rows

include_relationships = true           # Include FK edges
                                       # true = graph structure preserved

split_ratios = [0.8, 0.1, 0.1]        # Train/val/test split

split_seed = 42                        # Random seed for splitting
```

### Training Settings (`[training]`)

```toml
show_progress = true         # Display progress bar
compute_metrics = true       # Compute evaluation metrics
metric_interval = 1          # Metric frequency (epochs)
```

### Checkpoint Settings (`[checkpoint]`)

```toml
checkpoint_dir = ".gfs/checkpoints"
checkpoint_interval = 2      # Save every N epochs (0 = disabled)
max_checkpoints = 5          # Keep only last N checkpoints
```

### Evaluation Settings (`[evaluation]`)

```toml
metrics = ["Precision", "Recall", "MRR", "NDCG"]
top_k_values = [1, 5, 10]    # Compute metrics at these K values
```

---

## Example Workflows

### Workflow 1: Quick Exploration

```bash
# 1. Fast training on subset
gfs train start \
  --output models/explore \
  --config examples/train_config_fast.toml \
  --row-limit 1000 \
  --skip-metrics

# 2. Check if it works
cat models/explore.gnodes | head -20

# 3. If looks good, scale up
gfs train start \
  --output models/full \
  --config examples/train_config.toml
```

### Workflow 2: Hyperparameter Tuning

```bash
# Try different dimensions
for dim in 64 128 256; do
  # Create custom config
  sed "s/dimension = 128/dimension = $dim/" examples/train_config.toml > config_dim${dim}.toml

  # Train
  gfs train start --output models/model_dim${dim} --config config_dim${dim}.toml

  # Compare results
  tail -1 models/model_dim${dim}.metrics.csv
done
```

### Workflow 3: Incremental Development

```bash
# Phase 1: Dev model (fast iteration)
gfs train start --output models/dev_v1 --config examples/train_config_fast.toml --row-limit 2000

# Phase 2: Tuned dev model
# ... adjust config based on phase 1 ...
gfs train start --output models/dev_v2 --config examples/train_config_fast_tuned.toml --row-limit 5000

# Phase 3: Production model (full quality)
gfs train start --output models/prod_v1 --config examples/train_config_quality.toml

# Phase 4: Monitor production
cat models/prod_v1.metrics.csv
```

---

## Creating Custom Configurations

### Start from Template

```bash
# Copy and customize
cp examples/train_config.toml my_config.toml
nano my_config.toml

# Train with custom config
gfs train start --output models/custom --config my_config.toml
```

### Common Customizations

#### For Small Datasets (<5k rows)
```toml
[model.w2v]
dimension = 64
epochs = 5
num_threads = 4

[model.walker]
num_walks = 5
walk_length = 10

[training]
compute_metrics = false
```

#### For Large Datasets (>100k rows)
```toml
[model.w2v]
dimension = 256
epochs = 20
learning_rate = 0.015
num_threads = 16

[model.walker]
num_walks = 20
walk_length = 40

[checkpoint]
checkpoint_interval = 5
max_checkpoints = 3
```

#### For Text-Heavy Data
```toml
[model.w2v]
dimension = 256
window = 8              # Larger context for text
min_count = 2           # Filter rare words

[model.walker]
num_walks = 15
walk_length = 30
```

#### For Graph-Heavy Data (many relationships)
```toml
[data]
include_relationships = true

[model.walker]
num_walks = 20          # More walks to explore graph
walk_length = 40        # Longer walks for structure

[model.w2v]
dimension = 256
```

---

## Configuration Tips

### Memory Optimization

If you encounter out-of-memory errors:

1. **Reduce dimension**: `dimension = 64`
2. **Limit data**: `row_limit = 5000`
3. **Fewer walks**: `num_walks = 5, walk_length = 10`
4. **Disable checkpoints**: `checkpoint_interval = 0`

### Speed Optimization

For faster training:

1. **More threads**: `num_threads = 16`
2. **Fewer epochs**: `epochs = 5`
3. **Skip metrics**: `compute_metrics = false`
4. **Smaller dimension**: `dimension = 64`
5. **Disable checkpoints**: `checkpoint_interval = 0`

### Quality Optimization

For better embeddings:

1. **Larger dimension**: `dimension = 256`
2. **More epochs**: `epochs = 20`
3. **More walks**: `num_walks = 20, walk_length = 40`
4. **Lower learning rate**: `learning_rate = 0.015`
5. **Better HNSW**: `m = 32, ef_construction = 400`

---

## Validation

### Check Your Configuration

Before long training runs, validate your config:

```bash
# Quick test with row limit
gfs train start \
  --output models/test \
  --config my_config.toml \
  --row-limit 100 \
  --skip-metrics

# If it works, run full training
gfs train start \
  --output models/full \
  --config my_config.toml
```

### Compare Configurations

```bash
# Train with different configs
gfs train start --output models/config_a --config config_a.toml
gfs train start --output models/config_b --config config_b.toml

# Compare metrics
echo "=== Config A ==="
tail -1 models/config_a.metrics.csv

echo "=== Config B ==="
tail -1 models/config_b.metrics.csv
```

---

## See Also

- [Training Quick Start](../docs/TRAINING-QUICKSTART.md)
- [Full Training Guide](../docs/guides/training-gnn-models.md)
- [GFS CLI Reference](../docs/cli-reference.md)
