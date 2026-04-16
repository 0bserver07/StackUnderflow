#!/bin/bash

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "Running linters for StackUnderflow project..."
echo

echo "Python linters:"
echo "==============="

check_python_linter() {
    if ! command -v $1 &> /dev/null; then
        echo -e "${RED}$1 is not installed. Install with: pip install $1${NC}"
        return 1
    fi
    return 0
}

if check_python_linter "ruff"; then
    echo -e "${YELLOW}Running ruff linting...${NC}"
    ruff check stackunderflow/ --fix 2>&1 | grep -v -E "warning: The top-level linter settings|'select' -> 'lint.select'|'per-file-ignores' -> 'lint.per-file-ignores'"
    echo

    echo -e "${YELLOW}Running ruff formatting check...${NC}"
    ruff format stackunderflow/ --check --diff
    echo
fi

if check_python_linter "mypy"; then
    echo -e "${YELLOW}Running mypy...${NC}"
    mypy stackunderflow/ --ignore-missing-imports
    echo
fi

echo -e "${GREEN}Linting complete.${NC}"
echo

echo "Summary:"
echo "========"

PYTHON_ISSUES=$(ruff check stackunderflow/ 2>&1 | grep "Found" | tail -1 | grep -oE '[0-9]+ errors' | grep -oE '[0-9]+' || echo "0")
echo -e "Python (Ruff): ${YELLOW}$PYTHON_ISSUES errors${NC}"

MYPY_ISSUES=$(mypy stackunderflow/ --ignore-missing-imports 2>&1 | grep "Found" | grep -oE '[0-9]+ errors' | grep -oE '[0-9]+' || echo "0")
echo -e "Python (MyPy): ${YELLOW}$MYPY_ISSUES type errors${NC}"

echo
echo "To fix issues automatically:"
echo "  - Python formatting: ruff format stackunderflow/"
echo "  - Python linting:    ruff check stackunderflow/ --fix"
