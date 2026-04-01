#!/usr/bin/env python3
"""
Test runner for StackUnderflow project.
Run all tests with various options and generate reports.
"""

import sys
import os
import subprocess
import argparse
import time
from datetime import datetime


class Colors:
    """ANSI color codes for terminal output"""
    HEADER = '\033[95m'
    BLUE = '\033[94m'
    CYAN = '\033[96m'
    GREEN = '\033[92m'
    YELLOW = '\033[93m'
    RED = '\033[91m'
    ENDC = '\033[0m'
    BOLD = '\033[1m'
    UNDERLINE = '\033[4m'


def print_header(text):
    """Print a formatted header"""
    print(f"\n{Colors.HEADER}{Colors.BOLD}{'=' * 60}{Colors.ENDC}")
    print(f"{Colors.HEADER}{Colors.BOLD}{text.center(60)}{Colors.ENDC}")
    print(f"{Colors.HEADER}{Colors.BOLD}{'=' * 60}{Colors.ENDC}\n")


def print_section(text):
    """Print a section header"""
    print(f"\n{Colors.CYAN}{Colors.BOLD}→ {text}{Colors.ENDC}")


def run_command(cmd, description=None):
    """Run a command and capture output"""
    if description:
        print_section(description)
    
    print(f"{Colors.BLUE}$ {' '.join(cmd)}{Colors.ENDC}")
    
    start_time = time.time()
    result = subprocess.run(cmd, capture_output=True, text=True)
    duration = time.time() - start_time
    
    if result.returncode == 0:
        print(f"{Colors.GREEN}✓ Success ({duration:.2f}s){Colors.ENDC}")
    else:
        print(f"{Colors.RED}✗ Failed ({duration:.2f}s){Colors.ENDC}")
        if result.stderr:
            print(f"{Colors.RED}Error: {result.stderr}{Colors.ENDC}")
    
    return result


def run_tests(args):
    """Run tests based on arguments"""
    # Base pytest command
    cmd = [sys.executable, "-m", "pytest"]
    
    # Add test directory or specific file
    if args.file:
        cmd.append(args.file)
    elif args.module:
        if args.module == "processor":
            cmd.append("tests/stackunderflow/core/test_processor.py")
        elif args.module == "stats":
            cmd.append("tests/stackunderflow/core/test_stats.py")
        elif args.module == "memory_cache":
            cmd.append("tests/stackunderflow/utils/test_memory_cache.py")
        elif args.module == "verification":
            cmd.append("tests/stackunderflow/test_processor_data_verification.py")
        elif args.module == "performance":
            cmd.append("tests/stackunderflow/test_performance.py")
        elif args.module == "admin":
            cmd.append("tests/stackunderflow-site/test_admin.py")
        else:
            print(f"{Colors.RED}Unknown module: {args.module}{Colors.ENDC}")
            return False
    else:
        # Default: run all tests except performance
        if not args.include_performance:
            cmd.extend(["tests/", "--ignore=tests/stackunderflow/test_performance.py"])
        else:
            cmd.append("tests/")
    
    # Add options
    if args.verbose:
        cmd.extend(["-v", "-s"])
    else:
        cmd.append("-v")
    
    if args.stop_on_failure:
        cmd.append("-x")
    
    if args.failed_first:
        cmd.append("--ff")
    
    if args.pdb:
        cmd.append("--pdb")
    
    if args.coverage:
        cmd.extend([
            "--cov=stackunderflow",
            "--cov-report=term-missing",
            "--cov-report=html"
        ])
    
    if args.markers:
        cmd.extend(["-m", args.markers])
    
    if args.keyword:
        cmd.extend(["-k", args.keyword])
    
    # Run the tests
    result = run_command(cmd, "Running tests")
    
    # Print output
    if result.stdout:
        print(result.stdout)
    
    return result.returncode == 0


def run_linting(args):
    """Run linting checks"""
    print_header("Running Linting Checks")
    
    success = True
    
    # Check if ruff is installed
    ruff_check = subprocess.run(["which", "ruff"], capture_output=True)
    if ruff_check.returncode == 0:
        # Run ruff
        result = run_command(
            ["ruff", "check", "stackunderflow/", "tests/"],
            "Running ruff linter"
        )
        if result.returncode != 0:
            success = False
            print(result.stdout)
    else:
        print(f"{Colors.YELLOW}⚠ Ruff not installed. Skipping linting.{Colors.ENDC}")
        print(f"  Install with: pip install ruff")
    
    return success


def run_type_checking(args):
    """Run type checking"""
    print_header("Running Type Checking")
    
    # Check if mypy is installed
    mypy_check = subprocess.run(["which", "mypy"], capture_output=True)
    if mypy_check.returncode != 0:
        print(f"{Colors.YELLOW}⚠ Mypy not installed. Skipping type checking.{Colors.ENDC}")
        print(f"  Install with: pip install mypy")
        return True
    
    result = run_command(
        ["mypy", "stackunderflow/", "--ignore-missing-imports"],
        "Running mypy type checker"
    )
    
    if result.stdout:
        print(result.stdout)
    
    return result.returncode == 0


def generate_report(args):
    """Generate test report"""
    print_header("Test Report")
    
    # Run tests with junit output
    cmd = [
        sys.executable, "-m", "pytest", "tests/",
        "--tb=short",
        "--junit-xml=test_report.xml"
    ]
    
    # Exclude performance tests unless requested
    if not args.include_performance:
        cmd.append("--ignore=tests/stackunderflow/test_performance.py")
    
    if args.coverage:
        cmd.extend([
            "--cov=stackunderflow",
            "--cov-report=term",
            "--cov-report=html",
            "--cov-report=xml"
        ])
    
    result = run_command(cmd, "Generating test report")
    
    if result.returncode == 0:
        print(f"\n{Colors.GREEN}Reports generated:{Colors.ENDC}")
        print("  - test_report.xml (JUnit format)")
        if args.coverage:
            print("  - htmlcov/index.html (Coverage HTML report)")
            print("  - coverage.xml (Coverage XML report)")
            
            # Try to open coverage report
            if args.open_coverage:
                import webbrowser
                coverage_path = os.path.join(os.getcwd(), "htmlcov", "index.html")
                if os.path.exists(coverage_path):
                    webbrowser.open(f"file://{coverage_path}")
                    print(f"\n{Colors.GREEN}Opened coverage report in browser{Colors.ENDC}")
    
    return result.returncode == 0


def main():
    """Main entry point"""
    parser = argparse.ArgumentParser(
        description="Run tests for StackUnderflow project",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  python run_tests.py                    # Run all tests (except performance)
  python run_tests.py -p                 # Run all tests including performance
  python run_tests.py -v                 # Run with verbose output
  python run_tests.py -c                 # Run with coverage
  python run_tests.py -m processor       # Run only processor tests
  python run_tests.py -m performance     # Run only performance tests
  python run_tests.py -k "streaming"     # Run tests matching keyword
  python run_tests.py --all              # Run all checks (tests, lint, type)
  python run_tests.py --report           # Generate test report
        """
    )
    
    # Test selection
    parser.add_argument("-m", "--module", 
                        choices=["processor", "stats", "memory_cache", "verification", "performance", "admin"],
                        help="Run tests for specific module")
    parser.add_argument("-f", "--file", 
                        help="Run specific test file")
    parser.add_argument("-k", "--keyword",
                        help="Run tests matching keyword expression")
    
    # Test options
    parser.add_argument("-v", "--verbose", action="store_true",
                        help="Verbose output")
    parser.add_argument("-x", "--stop-on-failure", action="store_true",
                        help="Stop on first failure")
    parser.add_argument("--ff", "--failed-first", action="store_true",
                        dest="failed_first",
                        help="Run failed tests first")
    parser.add_argument("--pdb", action="store_true",
                        help="Drop into debugger on failures")
    parser.add_argument("--markers",
                        help="Run tests matching given mark expression")
    
    # Coverage
    parser.add_argument("-c", "--coverage", action="store_true",
                        help="Run with coverage report")
    parser.add_argument("--open-coverage", action="store_true",
                        help="Open coverage report in browser")
    
    # Performance tests
    parser.add_argument("-p", "--include-performance", action="store_true",
                        help="Include performance tests (excluded by default)")
    
    # Additional checks
    parser.add_argument("-l", "--lint", action="store_true",
                        help="Run linting checks")
    parser.add_argument("-t", "--type-check", action="store_true",
                        help="Run type checking")
    parser.add_argument("-a", "--all", action="store_true",
                        help="Run all checks (tests, lint, type)")
    
    # Reporting
    parser.add_argument("-r", "--report", action="store_true",
                        help="Generate test report")
    
    args = parser.parse_args()
    
    # Header
    print_header("StackUnderflow Test Runner")
    print(f"Started at: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
    
    success = True
    
    # Run linting if requested
    if args.lint or args.all:
        if not run_linting(args):
            success = False
    
    # Run type checking if requested
    if args.type_check or args.all:
        if not run_type_checking(args):
            success = False
    
    # Run tests (unless only report is requested)
    if not args.report or args.all or not (args.lint or args.type_check):
        print_header("Running Tests")
        if not run_tests(args):
            success = False
    
    # Generate report if requested
    if args.report:
        if not generate_report(args):
            success = False
    
    # Summary
    print_header("Summary")
    if success:
        print(f"{Colors.GREEN}{Colors.BOLD}✓ All checks passed!{Colors.ENDC}")
    else:
        print(f"{Colors.RED}{Colors.BOLD}✗ Some checks failed!{Colors.ENDC}")
    
    print(f"\nCompleted at: {datetime.now().strftime('%Y-%m-%d %H:%M:%S')}")
    
    return 0 if success else 1


if __name__ == "__main__":
    sys.exit(main())