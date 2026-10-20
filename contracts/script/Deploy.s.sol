// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Script, console} from "forge-std/Script.sol";

import {IRailgun} from "../src/ShieldVault.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {TestToken} from "../test/utils/TestToken.sol";

/// forge script script/Deploy.s.sol --rpc-url sepolia --broadcast
///   ETH_SEPOLIA_RPC_URL and DEPLOYER_PRIVATE_KEY supplied through the environment
///   MIN_WINDOW         the least time an open leaves before t0, and from t0 to t1 (default 1 hour)
///   LATE_WINDOW        how long from t1 only the maker may refund a forward swap (default 24 hours)
///   RAILGUN            Railgun's proxy, for payouts into Railgun (default none)
///   DEPLOY_TEST_TOKEN  also deploy a mintable payout token, for testnets
contract Deploy is Script {
    function run() external returns (ZecSwap swaps, TestToken token) {
        uint256 minWindow = vm.envOr("MIN_WINDOW", uint256(1 hours));
        uint256 lateWindow = vm.envOr("LATE_WINDOW", uint256(24 hours));
        IRailgun railgun = IRailgun(vm.envOr("RAILGUN", address(0)));
        uint256 key = vm.envOr("DEPLOYER_PRIVATE_KEY", uint256(0));
        if (key == 0) vm.startBroadcast();
        else vm.startBroadcast(key);
        swaps = new ZecSwap(minWindow, lateWindow, railgun);
        if (vm.envOr("DEPLOY_TEST_TOKEN", false)) {
            token = new TestToken();
        }
        vm.stopBroadcast();
        console.log("ZecSwap", address(swaps));
        console.log("TestToken", address(token));
    }
}
