#!/bin/bash
#
# Complete GNN Training Example: Flight Data
#
# This script demonstrates the full workflow of training a GNN model on flight data:
# 1. Initialize GFS repository with PostgreSQL
# 2. Import flight data from benchmark
# 3. Commit initial state
# 4. Train GNN model
# 5. Display results
#
# Prerequisites:
# - GFS CLI installed
# - Docker running
# - Flight data available at: benchmarks/flight-data-300MB/demo-small-en-20170815.sql
#
# Usage:
#   ./examples/train_flight_data.sh [output_dir]
#
# Example:
#   ./examples/train_flight_data.sh my-flight-project

set -e  # Exit on error
set -u  # Exit on undefined variable

# Configuration
PROJECT_DIR="${1:-flight-gnn-demo}"
SQL_FILE="benchmarks/flight-data-300MB/demo-small-en-20170815.sql"
MODEL_NAME="flight_model_v1"
CONFIG_FILE="examples/train_config_flight_data.toml"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Helper functions
print_step() {
    echo -e "${BLUE}==>${NC} ${1}"
}

print_success() {
    echo -e "${GREEN}✓${NC} ${1}"
}

print_error() {
    echo -e "${RED}✗${NC} ${1}"
}

print_info() {
    echo -e "${YELLOW}ℹ${NC} ${1}"
}

# Check prerequisites
check_prerequisites() {
    print_step "Checking prerequisites..."

    # Check if gfs is installed
    if ! command -v gfs &> /dev/null; then
        print_error "GFS CLI not found. Please install GFS first."
        exit 1
    fi
    print_success "GFS CLI found: $(gfs version 2>&1 | head -1 || echo 'version unknown')"

    # Check if Docker is running
    if ! docker info &> /dev/null; then
        print_error "Docker is not running. Please start Docker first."
        exit 1
    fi
    print_success "Docker is running"

    # Check if SQL file exists
    if [ ! -f "$SQL_FILE" ]; then
        print_error "Flight data not found at: $SQL_FILE"
        print_info "Please ensure the benchmark data is available"
        exit 1
    fi
    print_success "Flight data found: $SQL_FILE"

    echo ""
}

# Main workflow
main() {
    echo ""
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║         GNN Training Example: Flight Data                    ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""

    check_prerequisites

    # Step 1: Initialize repository
    print_step "Step 1/5: Initializing GFS repository..."
    if [ -d "$PROJECT_DIR" ]; then
        print_error "Directory '$PROJECT_DIR' already exists"
        print_info "Please choose a different name or remove the existing directory"
        exit 1
    fi

    mkdir -p "$PROJECT_DIR"
    cd "$PROJECT_DIR"

    gfs init --database-provider postgres --database-version 17
    print_success "Repository initialized at $PROJECT_DIR"
    echo ""

    # Step 2: Import data
    print_step "Step 2/5: Importing flight data..."
    print_info "This may take 1-2 minutes depending on your system..."

    gfs import --file "../$SQL_FILE" --format sql
    print_success "Flight data imported successfully"
    echo ""

    # Step 3: Commit initial state
    print_step "Step 3/5: Creating initial commit..."
    gfs commit -m "Initial import of flight data"
    print_success "Initial state committed"
    echo ""

    # Step 4: Verify data
    print_step "Step 4/5: Verifying database schema..."
    print_info "Extracting schema metadata..."
    gfs schema extract --output schema.json --compact

    TABLE_COUNT=$(gfs query "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = 'public'" 2>/dev/null | tail -1 || echo "unknown")
    print_success "Database ready with $TABLE_COUNT tables"
    echo ""

    # Step 5: Train model
    print_step "Step 5/5: Training GNN model..."
    print_info "Configuration: ../$CONFIG_FILE"
    print_info "Output: models/$MODEL_NAME"
    print_info "This will take 1-3 minutes depending on your hardware..."
    echo ""

    gfs train start \
        --output "models/$MODEL_NAME" \
        --config "../$CONFIG_FILE"

    print_success "Training completed!"
    echo ""

    # Display results
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║                        RESULTS                                ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""

    print_step "Model Files:"
    ls -lh "models/${MODEL_NAME}."* 2>/dev/null || print_info "Model files created"
    echo ""

    print_step "Training Metrics (last epoch):"
    if [ -f "models/${MODEL_NAME}.metrics.csv" ]; then
        tail -1 "models/${MODEL_NAME}.metrics.csv"
    else
        print_info "No metrics file found (metrics may have been skipped)"
    fi
    echo ""

    print_step "Model Metadata:"
    MODEL_ARTIFACT=$(find .gfs/models -name "artifact.json" -type f 2>/dev/null | head -1)
    if [ -n "$MODEL_ARTIFACT" ] && [ -f "$MODEL_ARTIFACT" ]; then
        cat "$MODEL_ARTIFACT" | jq '{id, version, dimension: .config.dimension, tables: .training_data.tables, nodes: .training_data.num_nodes, edges: .training_data.num_edges, commit: .commit_hash}' 2>/dev/null || cat "$MODEL_ARTIFACT"
    else
        print_info "Model metadata: .gfs/models/"
    fi
    echo ""

    # Success summary
    echo "╔══════════════════════════════════════════════════════════════╗"
    echo "║                    ✓ SUCCESS!                                 ║"
    echo "╚══════════════════════════════════════════════════════════════╝"
    echo ""
    print_success "GNN model trained successfully!"
    echo ""
    print_info "Next steps:"
    echo "  1. View metrics:  cat models/${MODEL_NAME}.metrics.csv"
    echo "  2. Check status:  gfs status"
    echo "  3. View commits:  gfs log"
    echo "  4. Train again:   gfs train start --output models/${MODEL_NAME}_v2 --config ../$CONFIG_FILE"
    echo ""
    print_info "Project location: $(pwd)"
    echo ""
}

# Run main function
main

exit 0
