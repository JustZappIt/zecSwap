#!/usr/bin/env python3
"""Restrict ONLY localhost:12345 to root and Alloy. Preserve every other rule."""
import argparse
import pwd
import subprocess

CHAIN = "ZECSWAP_ALLOY_ADMIN"
JUMP = ["-o", "lo", "-d", "127.0.0.1/32", "-p", "tcp", "--dport", "12345", "-j", CHAIN]


def ipt(*args, check=True):
    return subprocess.run(["/usr/sbin/iptables", "-w", "5", *args], check=check, capture_output=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--remove", action="store_true")
    args = parser.parse_args()
    if args.remove:
        if ipt("-C", "OUTPUT", *JUMP, check=False).returncode == 0:
            ipt("-D", "OUTPUT", *JUMP)
        if ipt("-S", CHAIN, check=False).returncode == 0:
            ipt("-F", CHAIN)
            ipt("-X", CHAIN)
    else:
        uid = str(pwd.getpwnam("alloy").pw_uid)
        if ipt("-S", CHAIN, check=False).returncode != 0:
            ipt("-N", CHAIN)
        # Do not flush the live chain: build once, then verify exact ownership rules.
        rules = [["-m", "owner", "--uid-owner", user, "-j", "ACCEPT"] for user in ("0", uid)]
        rules.append(["-p", "tcp", "-j", "REJECT", "--reject-with", "tcp-reset"])
        for rule in rules:
            if ipt("-C", CHAIN, *rule, check=False).returncode != 0:
                ipt("-A", CHAIN, *rule)
        if ipt("-C", "OUTPUT", *JUMP, check=False).returncode != 0:
            ipt("-I", "OUTPUT", "1", *JUMP)
