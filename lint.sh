#!/bin/bash

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "🔍 Running linters for StackUnderflow project..."
echo

# Python linters
echo "📘 Python Linters:"
echo "=================="

# Check if Python linters are installed
check_python_linter() {
    if ! command -v $1 &> /dev/null; then
        echo -e "${RED}❌ $1 is not installed. Install with: pip install $1${NC}"
        return 1
    fi
    return 0
}

# Run ruff
if check_python_linter "ruff"; then
    echo -e "${YELLOW}Running ruff linting...${NC}"
    # Run ruff and filter out the deprecation warnings while preserving other output
    # Exclude build.py from linting
    ruff check stackunderflow/ stackunderflow-site/ --exclude="*/build.py" --fix 2>&1 | grep -v -E "warning: The top-level linter settings|'select' -> 'lint.select'|'per-file-ignores' -> 'lint.per-file-ignores'"
    echo
    
    echo -e "${YELLOW}Running ruff formatting check...${NC}"
    # Check formatting with ruff (replaces black)
    ruff format stackunderflow/ stackunderflow-site/ --exclude="*/build.py" --check --diff
    echo
fi

# Run mypy
if check_python_linter "mypy"; then
    echo -e "${YELLOW}Running mypy...${NC}"
    mypy stackunderflow/ stackunderflow-site/ --exclude="build.py" --ignore-missing-imports
    echo
fi

# JavaScript linters
# echo "📙 JavaScript Linters:"
# echo "======================"

# # Check if npm/npx is available
# if ! command -v npx &> /dev/null; then
#     echo -e "${RED}❌ npx is not installed. Install Node.js first.${NC}"
# else
#     # Check if eslint is installed
#     if [ ! -f "node_modules/.bin/eslint" ]; then
#         echo -e "${RED}❌ ESLint is not installed. Install with: npm install --save-dev eslint${NC}"
#     else
#         echo -e "${YELLOW}Running ESLint...${NC}"
#         npx eslint stackunderflow/static/js/ stackunderflow-site/static/js/ stackunderflow-site/functions/ --fix
#         echo
#     fi
# fi

echo -e "${GREEN}✅ Linting complete!${NC}"
echo

# Show summary of issues
echo "📊 Summary:"
echo "=========="

# Count Python issues
PYTHON_ISSUES=$(ruff check stackunderflow/ stackunderflow-site/ --exclude="*/build.py" 2>&1 | grep "Found" | tail -1 | grep -oE '[0-9]+ errors' | grep -oE '[0-9]+' || echo "0")
echo -e "Python (Ruff): ${YELLOW}$PYTHON_ISSUES errors${NC}"

# Count MyPy issues  
MYPY_ISSUES=$(mypy stackunderflow/ stackunderflow-site/ --exclude="build.py" --ignore-missing-imports 2>&1 | grep "Found" | grep -oE '[0-9]+ errors' | grep -oE '[0-9]+' || echo "0")
echo -e "Python (MyPy): ${YELLOW}$MYPY_ISSUES type errors${NC}"

# Count JavaScript issues
# if command -v npx &> /dev/null && [ -f "node_modules/.bin/eslint" ]; then
#     JS_OUTPUT=$(npx eslint stackunderflow/static/js/ stackunderflow-site/static/js/ stackunderflow-site/functions/ 2>&1 | grep "problems" | tail -1)
#     if [ -n "$JS_OUTPUT" ]; then
#         # Parse: ✖ 311 problems (176 errors, 135 warnings)
#         JS_ISSUES=$(echo "$JS_OUTPUT" | grep -oE '[0-9]+ problems' | grep -oE '[0-9]+')
#         JS_ERRORS=$(echo "$JS_OUTPUT" | grep -oE '[0-9]+ errors' | grep -oE '[0-9]+')
#         JS_WARNINGS=$(echo "$JS_OUTPUT" | grep -oE '[0-9]+ warnings' | grep -oE '[0-9]+')
#         echo -e "JavaScript: ${YELLOW}$JS_ISSUES problems${NC} ($JS_ERRORS errors, $JS_WARNINGS warnings)"
#     else
#         echo -e "JavaScript: ${GREEN}No issues found${NC}"
#     fi
# else
#     echo -e "JavaScript: ${RED}ESLint not installed${NC}"
# fi

echo
echo "To fix issues automatically:"
echo "  - Python formatting: ruff format stackunderflow/ stackunderflow-site/"
echo "  - Python linting: ruff check stackunderflow/ stackunderflow-site/ --fix"
echo "  - JavaScript: npx eslint stackunderflow/static/js/ stackunderflow-site/static/js/ stackunderflow-site/functions/ --fix"