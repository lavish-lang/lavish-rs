# lavish-rs

[![Build Status](https://travis-ci.org/lavish-lang/lavish-rs.svg?branch=master)](https://travis-ci.org/lavish-lang/lavish-rs)
![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)

An RPC runtime for lavish. Imported by code generated by [lavish-compiler](https://github.com/lavish-lang/lavish-compiler/)

## Design choices

Originally prototyped on top of futures 0.3 & romio, it is currently using
threads, mpsc channels and std::net so that:

  * It stays compatible with rust stable
  * It doesn't have so many experimental dependencies

It's entirely possible this decision will be revisited in the future.

