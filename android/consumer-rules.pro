# libzecswap finds these by name.
-keepclasseswithmembernames,includedescriptorclasses class xyz.justzappit.atomicswap.AtomicSwapNative {
    native <methods>;
}
-keep class xyz.justzappit.atomicswap.AtomicSwapException {
    <init>(java.lang.String);
}
