/*
 * proto/prometheus.h -- standard proto shim for prometheus.library.
 * See docs/pci-library.md section 1 for the interface's provenance.
 */

#ifndef PROTO_PROMETHEUS_H
#define PROTO_PROMETHEUS_H

#include <exec/types.h>

#ifndef LIBRARIES_PROMETHEUS_H
#include <libraries/prometheus.h>
#endif

extern struct Library *PrometheusBase;

#include <clib/prometheus_protos.h>

#ifdef __GNUC__
#include <inline/prometheus.h>
#endif

#endif /* PROTO_PROMETHEUS_H */
