/*
 * Copyright (c)2013-2021 ZeroTier, Inc.
 *
 * Use of this software is governed by the Business Source License included
 * in the LICENSE.TXT file in the project's root directory.
 *
 * Change Date: 2026-01-01
 *
 * On the date above, in accordance with the Business Source License, use
 * of this software will be governed by version 2.0 of the Apache License.
 */
/****/

#ifndef ZT_METER_HPP
#define ZT_METER_HPP

#include "Constants.hpp"
#include "Mutex.hpp"

#include <algorithm>

namespace ZeroTier {

/**
 * Transfer rate and total transferred amount meter
 *
 * This class is lock-free and thread-safe.
 *
 * This maintains a set of buckets numbered according to the current time
 * modulo TUNIT. Each bucket is incremented within that time window. When
 * the time moves on to a new bucket, its old contents are added to a
 * total accumulator and a new counter for that bucket starts.
 *
 * @tparam TUNIT Unit of time in milliseconds (default: 1000 for one second)
 * @tparam LSIZE Log size in units of time (default: 10 for 10s worth of data)
 */
template<int64_t TUNIT = 1000, unsigned long LSIZE = 10>
class Meter
{
public:
	/**
	 * Create and initialize a new meter
	 *
	 * @param now Start time
	 */
	ZT_INLINE Meter() noexcept :
		m_counts(),
		m_totalExclCounts(0),
		m_bucket(0)
	{}

	/**
	 * Add a measurement
	 *
	 * @param now Current time
	 * @param count Count of items (usually bytes)
	 */
	ZT_INLINE void log(const int64_t now, uint64_t count) noexcept
	{
		// We log by choosing a log bucket based on the current time in units modulo
		// the log size and then if it's a new bucket setting it or otherwise adding
		// to it.
		const unsigned long bucket = ((unsigned long)(now / TUNIT)) % LSIZE;
		if (m_bucket.exchange(bucket) != bucket) {
			m_totalExclCounts.fetch_add(m_counts[bucket].exchange(count));
		} else {
			m_counts[bucket].fetch_add(count);
		}
	}

	/**
	 * Get rate per TUNIT time
	 *
	 * @param now Current time
	 * @param rate Result parameter: rate in count/TUNIT
	 * @param total Total count for life of object
	 */
	ZT_INLINE void rate(double &rate, uint64_t &total) const noexcept
	{
		total = 0;
		for (unsigned long i = 0;i < LSIZE;++i)
			total += m_counts[i].load();
		rate = (double) total / (double) LSIZE;
		total += m_totalExclCounts.load();
	}

private:
	std::atomic<uint64_t> m_counts[LSIZE];
	std::atomic<uint64_t> m_totalExclCounts;
	std::atomic<unsigned long> m_bucket;
};

} // namespace ZeroTier

#endif
