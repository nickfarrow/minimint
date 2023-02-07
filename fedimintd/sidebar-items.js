window.SIDEBAR_ITEMS = {"constant":[["CLIENT_CONFIG","Client configuration file"],["CLIENT_CONNECT_FILE","Client connection string file"],["CODE_VERSION","Version of the server code (should be the same among peers)"],["CONSENSUS_CONFIG","Server consensus-only configurable file"],["DB_FILE","Database file name"],["ENCRYPTED_EXT",""],["JSON_EXT",""],["LOCAL_CONFIG","Server locally configurable file"],["PRIVATE_CONFIG","Server encrypted private keys file"],["SALT_FILE","Salt backup for combining with the private key"],["TLS_CERT","TLS public cert"],["TLS_PK","Encrypted TLS private keys"]],"fn":[["encrypted_json_read","Reads an encrypted json file into a struct"],["encrypted_json_write","Writes struct into an encrypted json file"],["plaintext_json_read","Reads a plaintext json file into a struct"],["plaintext_json_write","Writes struct into a plaintext json file"],["read_server_configs","Reads the server from the local, private, and consensus cfg files (private file encrypted)"],["write_nonprivate_configs","Writes the server into plaintext json configuration files (private keys not serialized)"]],"mod":[["distributedgen",""],["encrypt",""],["ui",""]]};