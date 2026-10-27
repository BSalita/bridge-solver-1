// The benchmark page: two suites, the analysis engine and the claim checker,
// each in its own module and worker. Only one runs at a time (./lock.js).

import '../assets/tokens.css'
import './style.css'
import './analysis.js'
import './claims.js'
