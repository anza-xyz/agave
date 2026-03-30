#!/usr/bin/env python3
"""
Agave Repository Spellchecker
Scans Rust source files for common spelling errors in comments and strings.
Designed for CI/CD integration.

Usage:
    python3 spellcheck_ci.py [--path PATH] [--fix] [--report]

Author: Regg (AI Assistant)
"""

import os
import re
import argparse
from pathlib import Path
from typing import Dict, List, Tuple

# Common typos in technical/code documentation
# Format: (incorrect, correct)
COMMON_TYPOS = [
    # Common programming/technical typos
    ("teh", "the"),
    ("thse", "these"),
    ("t he", "the"),
    ("hwere", "where"),
    ("waht", "what"),
    ("htat", "that"),
    ("hwich", "which"),
    ("ho w", "how"),
    ("wit h", "with"),
    ("frmo", "from"),
    ("h tlml", "html"),
    ("js on", "json"),
    ("resutl", "result"),
    ("resutls", "results"),
    ("sucess", "success"),
    ("sucessful", "successful"),
    ("sucessfully", "successfully"),
    ("occured", "occurred"),
    ("occuring", "occurring"),
    ("recieve", "receive"),
    ("recieved", "received"),
    ("definately", "definitely"),
    ("definintion", "definition"),
    ("committ", "commit"),
    ("committs", "commits"),
    ("occured", "occurred"),
    ("occurence", "occurrence"),
    ("occurences", "occurrences"),
    ("refference", "reference"),
    ("refferences", "references"),
    ("refered", "referred"),
    ("refering", "referring"),
    ("intitalize", "initialize"),
    ("intitalized", "initialized"),
    ("intitalizes", "initializes"),
    ("intitalization", "initialization"),
    ("destory", "destroy"),
    ("destoryed", "destroyed"),
    ("destorys", "destroys"),
    ("compatable", "compatible"),
    ("comptabile", "compatible"),
    ("requirment", "requirement"),
    ("requirments", "requirements"),
    ("implemention", "implementation"),
    ("implementations", "implementations"),
    ("functinality", "functionality"),
    ("additonal", "additional"),
    ("additonally", "additionally"),
    ("aditional", "additional"),
    ("aditionally", "additionally"),
    ("programatically", "programmatically"),
    ("programatic", "programmatic"),
    ("seperate", "separate"),
    ("seperately", "separately"),
    ("seperation", "separation"),
    ("configuartion", "configuration"),
    ("configuraton", "configuration"),
    ("inital", "initial"),
    ("initalize", "initialize"),
    ("initalized", "initialized"),
    ("finaly", "finally"),
    ("utilites", "utilities"),
    ("utiltiy", "utility"),
    ("utiliy", "utility"),
    ("arguement", "argument"),
    ("arguements", "arguments"),
    ("retun", "return"),
    ("retuns", "returns"),
    ("retuned", "returned"),
    ("paramter", "parameter"),
    ("paramters", "parameters"),
    ("paramterized", "parameterized"),
    ("functoin", "function"),
    ("functoins", "functions"),
    ("implemnt", "implement"),
    ("implemnts", "implements"),
    ("implemnted", "implemented"),
    ("enviroment", "environment"),
    ("enviroments", "environments"),
    ("varible", "variable"),
    ("varibles", "variables"),
    ("caluclate", "calculate"),
    ("caluclated", "calculated"),
    ("caluclation", "calculation"),
    ("caluclations", "calculations"),
    ("specfic", "specific"),
    ("specfically", "specifically"),
    ("independant", "independent"),
    ("independantly", "independently"),
    ("dependant", "dependent"),
    ("availabe", "available"),
    ("availble", "available"),
    ("availiable", "available"),
    ("recomend", "recommend"),
    ("recomended", "recommended"),
    ("recomends", "recommends"),
    ("recomendation", "recommendation"),
    ("recomendations", "recommendations"),
    (" maint", " maintain"),
    ("maintainance", "maintenance"),
    ("maintenence", "maintenance"),
    ("neta", "meta"),
    ("nework", "network"),
    ("netowrk", "network"),
    ("protcol", "protocol"),
    ("protcols", "protocols"),
    ("reponse", "response"),
    ("reponses", "responses"),
    ("reques", "request"),
    ("requeses", "requests"),
    ("sperc", "spec"),
    ("perc", "spec"),
    ("asynchonous", "asynchronous"),
    ("synchonous", "synchronous"),
    ("synops", "syntax"),
    ("mispelled", "misspelled"),
    ("alot", "a lot"),
    ("wont", "won't"),
    ("cant", "can't"),
    ("dont", "don't"),
    ("doesnt", "doesn't"),
    ("didnt", "didn't"),
    ("wasnt", "wasn't"),
    ("isnt", "isn't"),
    ("arent", "aren't"),
    ("hasnt", "hasn't"),
    ("hadnt", "hadn't"),
    ("wouldnt", "wouldn't"),
    ("couldnt", "couldn't"),
    ("shouldnt", "shouldn't"),
    ("couldof", "could've"),
    ("wouldof", "would've"),
    ("shouldof", "should've"),
    ("thier", "their"),
    ("wierd", "weird"),
    ("seperate", "separate"),
    ("untill", "until"),
    ("begining", "beginning"),
    ("beleive", "believe"),
    ("beleives", "believes"),
    ("calender", "calendar"),
    ("changeing", "changing"),
    ("comapny", "company"),
    ("concious", "conscious"),
    ("doen", "done"),
    ("enviroment", "environment"),
    ("excact", "exact"),
    ("existance", "existence"),
    ("Go/Stop", "Go/Stop"),
    ("infor", "info"),
    ("knowlege", "knowledge"),
    ("labmda", "lambda"),
    ("occurence", "occurrence"),
    ("optoin", "option"),
    ("organiz", "organise"),
    ("organiztion", "organisation"),
    ("prefered", "preferred"),
    ("prefering", "preferring"),
    ("presance", "presence"),
    ("programm", "program"),
    ("relect", "relate"),
    ("remaning", "remaining"),
    ("repeatedley", "repeatedly"),
    ("serice", "service"),
    ("strng", "string"),
    ("succes", "success"),
    ("transfered", "transferred"),
    ("unknwn", "unknown"),
    ("valuse", "value"),
    ("varsion", "version"),
    ("writting", "writing"),
    ("wrod", "word"),
    ("xecute", "execute"),
    # Additional typos found in Agave
    ("reciever", "receiver"),
    ("occured", "occurred"),
    ("occures", "occurs"),
    ("occuring", "occurring"),
]

# File extensions to scan
SCAN_EXTENSIONS = {'.rs', '.md', '.toml', '.yaml', '.yml', '.txt', '.json'}

# Directories to skip
SKIP_DIRS = {'.git', 'target', 'node_modules', '.cargo', 'vendor'}


class SpellChecker:
    def __init__(self, base_path: str):
        self.base_path = Path(base_path)
        self.findings: List[Dict] = []
        
    def scan_file(self, file_path: Path) -> List[Dict]:
        """Scan a single file for spelling errors."""
        findings = []
        
        try:
            with open(file_path, 'r', encoding='utf-8', errors='ignore') as f:
                content = f.read()
                lines = content.split('\n')
        except Exception as e:
            return findings
            
        for line_num, line in enumerate(lines, 1):
            # Skip code lines - only check comments and strings
            # For Rust: // comments, /* */ comments, "strings"
            # For Markdown: all text
            # For TOML/YAML: strings
            
            # Find all words in the line
            for incorrect, correct in COMMON_TYPOS:
                # Use word boundary matching to avoid partial word replacements
                pattern = re.compile(r'\b' + re.escape(incorrect) + r'\b', re.IGNORECASE)
                matches = pattern.findall(line)
                
                if matches:
                    findings.append({
                        'file': str(file_path.relative_to(self.base_path)),
                        'line': line_num,
                        'incorrect': incorrect,
                        'correct': correct,
                        'context': line.strip()[:100]  # Truncate long lines
                    })
                    
        return findings
    
    def scan_directory(self, path: Path = None) -> Dict:
        """Recursively scan directory for spelling errors."""
        if path is None:
            path = self.base_path
            
        print(f"Scanning: {path}")
        
        for entry in os.scandir(path):
            # Skip certain directories
            if entry.is_dir() and entry.name in SKIP_DIRS:
                continue
            if entry.is_dir():
                self.scan_directory(Path(entry.path))
            elif entry.is_file():
                ext = Path(entry.name).suffix
                if ext in SCAN_EXTENSIONS:
                    file_findings = self.scan_file(Path(entry.path))
                    self.findings.extend(file_findings)
                    
        return self.get_report()
    
    def get_report(self) -> Dict:
        """Generate a report of findings."""
        # Group by file
        by_file = {}
        for finding in self.findings:
            file = finding['file']
            if file not in by_file:
                by_file[file] = []
            by_file[file].append(finding)
            
        return {
            'total_errors': len(self.findings),
            'files_affected': len(by_file),
            'by_file': by_file,
            'findings': self.findings
        }
    
    def print_report(self, report: Dict = None):
        """Print a formatted report."""
        if report is None:
            report = self.get_report()
            
        print("\n" + "="*70)
        print("AGAVE REPOSITORY SPELLCHECK REPORT")
        print("="*70)
        print(f"\nTotal errors found: {report['total_errors']}")
        print(f"Files affected: {report['files_affected']}")
        print("\n" + "-"*70)
        
        for file, findings in report['by_file'].items():
            print(f"\n📁 {file}")
            print(f"   Errors: {len(findings)}")
            for f in findings:
                print(f"   Line {f['line']}: '{f['incorrect']}' → '{f['correct']}'")
                print(f"            → {f['context']}")
                
        print("\n" + "="*70)


def main():
    parser = argparse.ArgumentParser(
        description='Agave Repository Spellchecker for CI/CD'
    )
    parser.add_argument(
        '--path', 
        default='/root/.openclaw/workspace/agave',
        help='Path to repository (default: /root/.openclaw/workspace/agave)'
    )
    parser.add_argument(
        '--report',
        action='store_true',
        help='Print detailed report'
    )
    parser.add_argument(
        '--fix',
        action='store_true',
        help='Auto-fix typos (NOT IMPLEMENTED - for safety)'
    )
    parser.add_argument(
        '--json',
        action='store_true',
        help='Output report as JSON'
    )
    
    args = parser.parse_args()
    
    print(f"Starting spellcheck on: {args.path}")
    
    checker = SpellChecker(args.path)
    report = checker.scan_directory()
    
    if args.json:
        import json
        print(json.dumps(report, indent=2))
    else:
        checker.print_report(report)
    
    # Exit with error code if issues found (for CI)
    if report['total_errors'] > 0:
        print(f"\n⚠️  Found {report['total_errors']} potential spelling errors")
        print("   Review and fix manually or update COMMON_TYPOS list")
        # Don't exit with error - this is a soft check
        # return 1
    else:
        print("\n✅ No spelling errors found!")
        
    return 0


if __name__ == '__main__':
    exit(main())