```mermaid
stateDiagram-v2
    classDef Trusted stroke:green;
    classDef Untrusted stroke:red;
    class Verified Trusted
    class Parsed Untrusted
    class Admissible Untrusted
    class Bytes Untrusted
    [*] --> Bytes
    Bytes --> Parsed : parse_turbine(bytes)
    Bytes --> Parsed : parse_repair(bytes)
    Parsed --> Admissible : check_policy(policy)
    Admissible --> Verified : verify(leader)
    Verified --> Verified : resign()
    Parsed --> [*] : Reject
    Admissible --> [*] : Reject
    Bytes --> Verified :  blockstore read
```
