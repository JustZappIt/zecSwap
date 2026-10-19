# The mainnet gateway from deploy/nginx.conf: zones and variables of its own, on its own port, to
# the mainnet services. install.sh and enable-deposits.sh make the site with it.
s#\$zecswap_#$zecswap_mainnet_#g
s#zone=zecswap_#zone=zecswap_mainnet_#g
s#limit_conn zecswap_#limit_conn zecswap_mainnet_#g
s#listen 127\.0\.0\.1:8080;#listen 127.0.0.1:8090;#
s#127\.0\.0\.1:8787#127.0.0.1:8797#g
s#127\.0\.0\.1:8788#127.0.0.1:8798#g
s#127\.0\.0\.1:8789#127.0.0.1:8799#g
