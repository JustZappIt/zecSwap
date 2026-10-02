// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Script} from "forge-std/Script.sol";
import {ZecSwap} from "../src/ZecSwap.sol";
import {IERC20} from "../src/Token.sol";

/// Move the stopped testnet maker's available inventory; never touches locked swaps.
contract RotateTestnetInventory is Script {
    function run() external {
        require(block.chainid == 11155111, "Sepolia only");
        uint256 key = vm.envUint("MAKER_PRIVATE_KEY");
        address maker = vm.addr(key);
        require(maker == vm.envAddress("EXPECTED_MAKER"), "wrong maker key");
        ZecSwap previous = ZecSwap(vm.envAddress("OLD_CONTRACT"));
        ZecSwap next = ZecSwap(vm.envAddress("NEW_CONTRACT"));
        address token = vm.envAddress("PAYOUT_TOKEN");
        require(address(previous) != address(next), "same deployment");
        require(next.LOCK_DURATION() == previous.LOCK_DURATION(), "lock duration differs");
        require(address(next.RAILGUN()) == address(previous.RAILGUN()), "Railgun differs");
        require(next.rescueNonces(bytes32(0)) == 0, "unexpected rescue state");
        uint256 available = previous.balanceOf(maker, token);
        vm.startBroadcast(key);
        if (available != 0) previous.withdraw(token, available, maker);
        uint256 funding = IERC20(token).balanceOf(maker);
        if (funding != 0) {
            require(IERC20(token).approve(address(next), funding), "approve failed");
            next.deposit(token, funding);
        }
        vm.stopBroadcast();
    }
}
