#!/bin/bash

# Colores para la terminal
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}=== Bittice vs MySQL Performance Benchmark ===${NC}\n"

function run_benchmark() {
    local title="$1"
    local mysql_query="$2"
    local bittice_url="$3"

    echo -e "${GREEN}Test: $title${NC}"
    
    # --- MySQL ---
    start=$(date +%s%N)
    docker exec mysql-sakila mysql -uroot -psakila -e "$mysql_query" > /dev/null 2>&1
    end=$(date +%s%N)
    mysql_ms=$(( (end - start) / 1000000 ))
    echo "  MySQL:   ${mysql_ms}ms"

    # --- Bittice ---
    start=$(date +%s%N)
    curl -s "$bittice_url" > /dev/null
    end=$(date +%s%N)
    bittice_ms=$(( (end - start) / 1000000 ))
    echo "  Bittice: ${bittice_ms}ms"
    
    echo ""
}

# 1. Tabla pequeña (Actor ~200 filas)
run_benchmark "Full Scan Table 'actor' (~200 rows)" \
    "SELECT actor_id, first_name, last_name FROM sakila.actor;" \
    "http://localhost:3000/search?limit=1000"

# 2. Tabla mediana (Payment ~16,000 filas)
run_benchmark "Full Scan Table 'payment' (~16,000 rows)" \
    "SELECT payment_id, customer_id, amount, payment_date FROM sakila.payment;" \
    "http://localhost:3000/search_payment"

# 3. Filtro específico
run_benchmark "Filter by ID (payment_id=500)" \
    "SELECT * FROM sakila.payment WHERE payment_id = 500;" \
    "http://localhost:3000/search_payment?f=payment_id:Eq:500"

echo -e "${BLUE}Benchmark complete.${NC}"
