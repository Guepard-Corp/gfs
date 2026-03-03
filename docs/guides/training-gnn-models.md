# Training GNN Models on Database Data

This guide demonstrates how to train Graph Neural Network (GNN) models on your database using GFS. We'll use the flight data benchmark as an example.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Setup Database](#setup-database)
3. [Prepare Training Configuration](#prepare-training-configuration)
4. [Train the Model](#train-the-model)
5. [Understanding the Output](#understanding-the-output)
6. [Model Management](#model-management)
7. [Advanced Usage](#advanced-usage)

---

## Prerequisites

- GFS CLI installed and available in your PATH
- Docker running (for database compute)
- Benchmark data available at `benchmarks/flight-data-300MB/demo-small-en-20170815.sql`

---

## Setup Database

### Step 1: Initialize a GFS Repository with PostgreSQL

```bash
# Create a new directory for your project
mkdir my-flight-analysis
cd my-flight-analysis

# Initialize GFS with PostgreSQL 17
gfs init --database-provider postgres --database-version 17
```

**Expected output:**
```
Initializing GFS repository at /path/to/my-flight-analysis
✓ Created .gfs directory structure
✓ Initialized repository metadata
✓ Registered database provider: postgres:17
✓ Starting database container...
✓ Database ready at localhost:5432
```

### Step 2: Import the Flight Data

```bash
# Import the benchmark SQL file
gfs import --file ../benchmarks/flight-data-300MB/demo-small-en-20170815.sql --format sql
```

**Expected output:**
```
Importing data from ../benchmarks/flight-data-300MB/demo-small-en-20170815.sql
Format: sql
Database provider: postgres
✓ Connected to database
✓ Import completed successfully
```

### Step 3: Verify the Data

```bash
# Check database status and connection info
gfs status

# Explore the schema
gfs schema extract --output schema.json

# Query the database to see what tables were created
gfs query "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public'"
```

### Step 4: Create Your First Commit

```bash
# Commit the initial database state
gfs commit -m "Initial import of flight data"
```

**Expected output:**
```
Extracting database schema...
Creating commit...
✓ Commit created: a1b2c3d4e5f6... (Initial import of flight data)
```

---

## Prepare Training Configuration

### Create Training Configuration File

Create a file named `train_config.toml` in your project directory:

```bash
cat > train_config.toml << 'EOF'
# Model configuration
[model.w2v]
dimension = 128              # Embedding dimension (128 or 256 recommended)
epochs = 10                  # Training epochs
learning_rate = 0.025        # Learning rate
window = 5                   # Context window size
negative_samples = 15        # Negative sampling count
min_count = 1                # Minimum word frequency
num_threads = 8              # Parallel threads

[model.hnsw]
m = 16                       # HNSW construction parameter
ef_construction = 200        # Build-time search depth
ef = 200                     # Query-time search depth

[model.walker]
num_walks = 10               # Random walks per node
walk_length = 20             # Steps per walk
seed = 42                    # Random seed for reproducibility

# Data configuration
[data]
# Leave empty to include all tables, or specify:
# table_filter = ["flights", "airports", "airlines"]
row_limit = 10000            # Limit rows per table (remove for all data)
include_relationships = true # Include foreign key relationships as edges
split_ratios = [0.8, 0.1, 0.1]  # Train/validation/test split
split_seed = 42              # Seed for data splitting

# Training parameters
[training]
show_progress = true         # Show progress bar
compute_metrics = true       # Compute evaluation metrics
metric_interval = 1          # Compute metrics every N epochs

# Checkpointing
[checkpoint]
checkpoint_dir = ".gfs/checkpoints"
checkpoint_interval = 2      # Save checkpoint every N epochs (0 = disabled)
max_checkpoints = 5          # Keep only last N checkpoints

# Evaluation metrics
[evaluation]
metrics = ["Precision", "Recall", "MRR", "NDCG"]
top_k_values = [1, 5, 10]    # Compute metrics for top-K results
EOF
```

---

## Train the Model

### Basic Training Command

```bash
# Train a model on all tables
gfs train start \
  --output models/flight_model_v1 \
  --config train_config.toml

cargo run --bin gfs train start \
  --output models/flight_model_v1 \
  --row-limit 5000 \
  --config train_config.toml
   -d demo
```

### Training with Specific Tables

If you only want to train on specific tables (e.g., flights, airports, airlines):

```bash
gfs train start \
  --output models/flight_model_v1 \
  --config train_config.toml \
  --tables flights,airports,airlines
```

### Training with Row Limit Override

To quickly test with limited data:

```bash
gfs train start \
  --output models/flight_model_test \
  --config train_config.toml \
  --row-limit 1000 \
  --skip-metrics
```

**Expected output during training:**
```
Loading training configuration from train_config.toml...
Extracting database schema...
Found 5 tables
Building graph from database...
Fetching data from table: flights (10000 rows)
Fetching data from table: airports (500 rows)
Fetching data from table: airlines (100 rows)
Extracting foreign key relationships...
Graph built: 10600 nodes, 25000 edges
Splitting data (train/val/test: [0.8, 0.1, 0.1])...
Split complete: 8480 train, 1060 val, 1060 test nodes

Starting training...
Building vocabulary...
Vocabulary size: 15420 words
Training Word2Vec...
Epoch 1/10: 100% [===================] 3.2s | Loss: 0.245
Epoch 2/10: 100% [===================] 3.1s | Loss: 0.198
...
Epoch 10/10: 100% [===================] 2.9s | Loss: 0.087

Building HNSW index...
✓ Index built: 10600 nodes indexed

Saving model to models/flight_model_v1...
✓ Model saved

Metrics exported to models/flight_model_v1.metrics.csv

============================================================
Training complete!
============================================================
  Model dimension:  128
  Vocabulary size:  15420
  Corpus size:      10600
  Training time:    32.50s
============================================================

Model files created:
  - models/flight_model_v1.w2v   (Word2Vec embeddings)
  - models/flight_model_v1.hnsw  (HNSW search index)
  - models/flight_model_v1.gnodes (Node metadata)
  - models/flight_model_v1.metrics.csv (Training metrics)

You can now use this model for semantic search!
```

---

## Understanding the Output

### Model Files

After training, you'll have four files:

1. **`flight_model_v1.w2v`** - Word2Vec embeddings
   - Contains the trained word embeddings
   - Binary format for fast loading
   - Size depends on vocabulary and dimension

2. **`flight_model_v1.hnsw`** - HNSW search index
   - Approximate nearest neighbor index
   - Enables fast semantic search
   - Built from node embeddings

3. **`flight_model_v1.gnodes`** - Node metadata
   - Maps node indices to database rows
   - Contains provenance information
   - Includes source table and row ID

4. **`flight_model_v1.metrics.csv`** - Training metrics
   - Epoch-by-epoch performance
   - Precision@K, Recall@K, MRR, NDCG
   - Used for model evaluation

### Model Registry

The model is automatically registered in `.gfs/models/` with metadata:

```bash
# View model metadata
cat .gfs/models/gnn_*/artifact.json
```

**Example artifact.json:**
```json
{
  "id": "gnn_1709440123_a1b2c3d4",
  "version": "1.0.0",
  "name": null,
  "description": null,
  "trained_at": "2024-03-03T00:22:03Z",
  "trained_by": "user@hostname",
  "config": {
    "dimension": 128,
    "epochs": 10,
    "learning_rate": 0.025,
    "hyperparameters": {}
  },
  "training_data": {
    "num_nodes": 10600,
    "num_edges": 25000,
    "tables": ["flights", "airports", "airlines"],
    "row_count": 10600,
    "split_ratios": [0.8, 0.1, 0.1]
  },
  "metrics": {
    "precision@5": 0.82,
    "recall@10": 0.91,
    "mrr": 0.78
  },
  "model_path": "models/flight_model_v1",
  "commit_hash": "a1b2c3d4e5f6789...",
  "tags": []
}
```

---

## Model Management

### Viewing Training Metrics

```bash
# View metrics CSV
cat models/flight_model_v1.metrics.csv
```

**Example output:**
```csv
epoch,precision@1,precision@5,precision@10,recall@1,recall@5,recall@10,mrr,ndcg@5,ndcg@10
1,0.65,0.72,0.76,0.12,0.58,0.78,0.68,0.69,0.72
2,0.71,0.78,0.81,0.15,0.64,0.83,0.74,0.75,0.78
...
10,0.79,0.85,0.88,0.18,0.72,0.91,0.82,0.83,0.86
```

You can plot these metrics using any tool:

```bash
# Using Python/pandas
python -c "
import pandas as pd
import matplotlib.pyplot as plt

df = pd.read_csv('models/flight_model_v1.metrics.csv')
df.plot(x='epoch', y=['precision@5', 'recall@10', 'mrr'])
plt.savefig('training_metrics.png')
print('Plot saved to training_metrics.png')
"
```

### Reproducing Models

Since models are linked to GFS commits, you can reproduce them:

```bash
# Check which commit the model was trained on
MODEL_COMMIT=$(jq -r '.commit_hash' .gfs/models/gnn_*/artifact.json)
echo "Model trained on commit: $MODEL_COMMIT"

# Checkout that commit
gfs checkout $MODEL_COMMIT

# Verify database state matches
gfs status

# Retrain to reproduce results
gfs train start \
  --output models/flight_model_v1_reproduced \
  --config train_config.toml
```

### Comparing Models

```bash
# Train model v2 with different hyperparameters
cat > train_config_v2.toml << 'EOF'
[model.w2v]
dimension = 256              # Increased dimension
epochs = 20                  # More epochs
learning_rate = 0.015        # Lower learning rate
# ... rest of config
EOF

gfs train start \
  --output models/flight_model_v2 \
  --config train_config_v2.toml

# Compare metrics
echo "=== Model V1 Metrics ==="
tail -1 models/flight_model_v1.metrics.csv

echo "=== Model V2 Metrics ==="
tail -1 models/flight_model_v2.metrics.csv
```

---

## Advanced Usage

### Resume Training from Checkpoint

If training is interrupted, you can resume from the last checkpoint:

```bash
gfs train resume \
  --checkpoint .gfs/checkpoints/checkpoint_epoch_0006 \
  --output models/flight_model_v1_continued
```

### Training on Updated Data

```bash
# Make changes to your database
gfs query "INSERT INTO flights VALUES (...)"

# Create a new commit
gfs commit -m "Added new flight data"

# Train a new model version on updated data
gfs train start \
  --output models/flight_model_v2 \
  --config train_config.toml

# The new model will be linked to the new commit
# You can compare performance across data versions
```

### Incremental Training Strategy

For large datasets, use an incremental approach:

```bash
# Step 1: Train on subset for quick iteration
gfs train start \
  --output models/flight_model_dev \
  --config train_config.toml \
  --row-limit 5000 \
  --skip-metrics

# Step 2: Tune hyperparameters on dev set
# ... adjust config based on dev results ...

# Step 3: Train final model on full dataset
gfs train start \
  --output models/flight_model_production \
  --config train_config_final.toml
  # No --row-limit = use all data
```

### Multi-Table Training Strategy

For databases with many tables, train specialized models:

```bash
# Model 1: Core flight operations
gfs train start \
  --output models/flights_core \
  --tables flights,airports,airlines \
  --config train_config.toml

# Model 2: Customer interactions
gfs train start \
  --output models/customers \
  --tables customers,bookings,reviews \
  --config train_config.toml

# Model 3: Full database
gfs train start \
  --output models/complete \
  --config train_config.toml
  # No --tables = all tables
```

### Performance Tuning

#### For Speed (Fast Training)

```toml
[model.w2v]
dimension = 64               # Smaller dimension
epochs = 5                   # Fewer epochs
num_threads = 16             # More threads

[model.walker]
num_walks = 5                # Fewer walks
walk_length = 10             # Shorter walks

[training]
compute_metrics = false      # Skip metrics

[checkpoint]
checkpoint_interval = 0      # Disable checkpoints
```

#### For Quality (Better Embeddings)

```toml
[model.w2v]
dimension = 256              # Larger dimension
epochs = 20                  # More epochs
learning_rate = 0.015        # Lower learning rate
window = 8                   # Larger context

[model.walker]
num_walks = 20               # More walks
walk_length = 40             # Longer walks

[model.hnsw]
m = 32                       # Better connectivity
ef_construction = 400        # More thorough build
```

---

## Troubleshooting

### Out of Memory

If you encounter memory issues:

```bash
# Reduce row limit
gfs train start \
  --output models/flight_model \
  --config train_config.toml \
  --row-limit 5000

# Or reduce model dimension in config
# dimension = 64
```

### Slow Training

```bash
# Increase threads in config
# num_threads = 16

# Reduce walks
# num_walks = 5
# walk_length = 10

# Skip metrics during training
gfs train start \
  --output models/flight_model \
  --config train_config.toml \
  --skip-metrics
```

### Poor Model Quality

If semantic search results are not good:

1. **Increase embedding dimension**: `dimension = 256`
2. **Train longer**: `epochs = 20`
3. **More random walks**: `num_walks = 20, walk_length = 40`
4. **Include more relationships**: Ensure `include_relationships = true`
5. **Use more data**: Remove or increase `row_limit`

---

## Next Steps

After training your model, you can:

1. **Use for semantic search** (coming soon):
   ```bash
   gfs search --model models/flight_model_v1 "flights to paris"
   ```

2. **Integrate with query engine** (RFC 006 - Hybrid Query Engine):
   - Combine SQL with semantic search
   - Enable natural language queries

3. **Predictions** (coming soon):
   ```bash
   # Link prediction (find missing relationships)
   gfs train evaluate --model models/flight_model_v1
   ```

4. **Export for external use**:
   - Models can be loaded in Python/Rust applications
   - Use `gfs-ai-models` library for inference

---

## Example: Complete Workflow

Here's a complete example from start to finish:

```bash
# 1. Initialize project
mkdir flight-analysis && cd flight-analysis
gfs init --database-provider postgres --database-version 17

# 2. Import data
gfs import --file ../benchmarks/flight-data-300MB/demo-small-en-20170815.sql

# 3. Commit initial state
gfs commit -m "Initial flight data import"

# 4. Create training config
cat > train_config.toml << 'EOF'
[model.w2v]
dimension = 128
epochs = 10
learning_rate = 0.025
window = 5
negative_samples = 15
min_count = 1
num_threads = 8

[model.hnsw]
m = 16
ef_construction = 200
ef = 200

[model.walker]
num_walks = 10
walk_length = 20
seed = 42

[data]
row_limit = 10000
include_relationships = true
split_ratios = [0.8, 0.1, 0.1]
split_seed = 42

[training]
show_progress = true
compute_metrics = true
metric_interval = 1

[checkpoint]
checkpoint_dir = ".gfs/checkpoints"
checkpoint_interval = 2
max_checkpoints = 5

[evaluation]
metrics = ["Precision", "Recall", "MRR", "NDCG"]
top_k_values = [1, 5, 10]
EOF

# 5. Train model
gfs train start \
  --output models/flight_model_v1 \
  --config train_config.toml

# 6. View results
cat models/flight_model_v1.metrics.csv
ls -lh models/flight_model_v1.*

# 7. Check model metadata
cat .gfs/models/gnn_*/artifact.json | jq .

echo "✓ Training complete! Model ready for use."
```

---

## References

- [GFS Training Infrastructure RFC](../rfcs/010-training-infrastructure.md) (if exists)
- [GraphSemanticEngine Documentation](../../crates/ai/models/README.md)
- [GFS CLI Reference](../cli-reference.md)
- [Word2Vec Paper](https://arxiv.org/abs/1301.3781)
- [DeepWalk Paper](https://arxiv.org/abs/1403.6652)
- [HNSW Paper](https://arxiv.org/abs/1603.09320)

---

**Questions or issues?** Open an issue at https://github.com/Guepard-Corp/gfs/issues
