# GNN Training Quick Start

Get a trained GNN model on your database in 5 minutes.

## Prerequisites

- GFS CLI installed
- Docker running
- Database file ready (we'll use `benchmarks/flight-data-300MB/demo-small-en-20170815.sql`)

## Quick Start (5 minutes)

### 1. Setup (30 seconds)

```bash
# Create project directory
mkdir my-ml-project && cd my-ml-project

# Initialize with PostgreSQL
gfs init --database-provider postgres --database-version 17
```

### 2. Import Data (1 minute)

```bash
# Import your database
gfs import --file ../benchmarks/flight-data-300MB/demo-small-en-20170815.sql

# Commit initial state
gfs commit -m "Initial data import"
```

### 3. Train Model (3 minutes)

```bash
# Create minimal config
cat > train.toml << 'EOF'
[model.w2v]
dimension = 128
epochs = 10

[data]
row_limit = 5000
split_ratios = [0.8, 0.1, 0.1]

[training]
show_progress = true
EOF

# Train!
gfs train start --output models/my_first_model --config train.toml
```

**That's it!** You now have a trained GNN model.

## What You Get

```
models/
  ├── my_first_model.w2v          # Embeddings
  ├── my_first_model.hnsw         # Search index
  ├── my_first_model.gnodes       # Node metadata
  └── my_first_model.metrics.csv  # Performance metrics
```

## View Results

```bash
# Check metrics
cat models/my_first_model.metrics.csv

# View model metadata
cat .gfs/models/gnn_*/artifact.json | jq .
```

## Next Steps

1. **Train on more data**: Remove `row_limit` from config
2. **Tune hyperparameters**: Increase `dimension` or `epochs`
3. **Train on specific tables**: Add `--tables flights,airports,airlines`
4. **Read full guide**: See [docs/guides/training-gnn-models.md](guides/training-gnn-models.md)

## Common Commands

```bash
# Train with specific tables
gfs train start --output models/model --config train.toml --tables table1,table2

# Train with limited data (for testing)
gfs train start --output models/test --config train.toml --row-limit 1000 --skip-metrics

# Resume from checkpoint
gfs train resume --checkpoint .gfs/checkpoints/checkpoint_epoch_0004 --output models/resumed

# Check database status
gfs status

# View schema
gfs schema extract
```

## Minimal Configuration File

Create `train.toml`:

```toml
[model.w2v]
dimension = 128          # Embedding size (64, 128, or 256)
epochs = 10              # Training iterations

[data]
row_limit = 5000         # Rows per table (remove for all)
split_ratios = [0.8, 0.1, 0.1]  # Train/val/test

[training]
show_progress = true     # Show progress bar
```

## Full Configuration Example

For production training, use this config:

```toml
[model.w2v]
dimension = 256
epochs = 20
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
# table_filter = ["flights", "airports"]  # Uncomment to limit tables
# row_limit = 10000                       # Uncomment to limit rows
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
```

## Troubleshooting

**Out of memory?**
```bash
# Reduce row limit
gfs train start --output models/model --config train.toml --row-limit 2000
```

**Too slow?**
```bash
# Skip metrics during training
gfs train start --output models/model --config train.toml --skip-metrics
```

**Poor quality?**
```bash
# Increase dimension and epochs in train.toml:
# dimension = 256
# epochs = 20
```

## Complete Example

```bash
#!/bin/bash
# complete-training-example.sh

set -e  # Exit on error

echo "🚀 Starting GNN training workflow..."

# 1. Setup
echo "📁 Setting up project..."
mkdir -p gnn-demo && cd gnn-demo
gfs init --database-provider postgres --database-version 17

# 2. Import data
echo "📥 Importing database..."
gfs import --file ../benchmarks/flight-data-300MB/demo-small-en-20170815.sql
gfs commit -m "Initial data import"

# 3. Create config
echo "⚙️  Creating training configuration..."
cat > train.toml << 'EOF'
[model.w2v]
dimension = 128
epochs = 10
learning_rate = 0.025

[data]
row_limit = 5000
include_relationships = true
split_ratios = [0.8, 0.1, 0.1]

[training]
show_progress = true
compute_metrics = true
EOF

# 4. Train
echo "🧠 Training model..."
gfs train start --output models/flight_model --config train.toml

# 5. Results
echo "✅ Training complete!"
echo ""
echo "📊 Metrics:"
cat models/flight_model.metrics.csv | tail -1
echo ""
echo "📁 Model files:"
ls -lh models/flight_model.*
echo ""
echo "🎉 Done! Your model is ready."
```

Save and run:
```bash
chmod +x complete-training-example.sh
./complete-training-example.sh
```

---

For detailed documentation, see [docs/guides/training-gnn-models.md](guides/training-gnn-models.md)
