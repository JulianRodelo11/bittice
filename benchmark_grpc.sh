#!/bin/bash

# Colores para la terminal
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}=== Bittice (gRPC) vs MySQL Performance Benchmark ===${NC}\n"

PROTO_DIR="/Users/juliancamilorodelopulido/Desktop/engine/bittice/proto"
PROTO_FILE="bittice.proto"

function run_benchmark() {
    local title="$1"
    local mysql_query="$2"
    local grpc_json="$3"

    echo -e "${GREEN}Test: $title${NC}"
    
    # --- MySQL ---
    start=$(date +%s%N)
    docker exec mysql-sakila mysql -uroot -psakila -e "$mysql_query" > /dev/null 2>&1
    end=$(date +%s%N)
    mysql_ms=$(( (end - start) / 1000000 ))
    echo "  MySQL:   ${mysql_ms}ms"

    # --- Bittice gRPC ---
    start=$(date +%s%N)
    grpcurl -plaintext -import-path "$PROTO_DIR" -proto "$PROTO_FILE" -d "$grpc_json" localhost:50051 bittice.Database/SearchUnary > /dev/null
    end=$(date +%s%N)
    bittice_ms=$(( (end - start) / 1000000 ))
    echo "  Bittice: ${bittice_ms}ms"
    
    echo ""
}

# 1. Actor (~200 filas)
run_benchmark "Scan Table 'actor' (~200 rows)" \
    "SELECT actor_id, first_name, last_name FROM sakila.actor;" \
    '{"entity": "sakila", "table": "actor", "selected_fields": ["actor_id", "first_name", "last_name"], "limit": 1000}'

# 2. Payment (~16,000 filas)
run_benchmark "Scan Table 'payment' (~16,000 rows)" \
    "SELECT payment_id, customer_id, amount FROM sakila.payment;" \
    '{"entity": "sakila", "table": "payment", "selected_fields": ["payment_id", "customer_id", "amount"], "limit": 20000}'

echo -e "${BLUE}Benchmark complete.${NC}"
