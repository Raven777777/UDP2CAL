package com.udp2cal.app

import org.junit.Assert.assertTrue
import org.junit.Test

class StreamResamplerTest {
    @Test
    fun process_fullBlockAtMinimumRatio_fitsOutputBuffer() {
        val resampler = StreamResampler(channels = 2)
        resampler.setSourceRate(sourceRate = 100, deviceRate = 1000)
        resampler.setAdjust(-0.005)

        val input = ShortArray(StreamResampler.CAPACITY_FRAMES * 2)
        val outputShorts = resampler.process(input, offset = 0, frames = StreamResampler.CAPACITY_FRAMES)

        assertTrue("minimum supported ratio should expand a full input block", outputShorts > 0)
        assertTrue("returned length must fit the exposed output buffer", outputShorts <= resampler.buffer().size)
    }
}
