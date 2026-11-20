#!/usr/bin/env swift
// Regenerates core/toolbox-core/tests/data/quadrants.h264, the stream the
// VideoToolbox decoder is tested with:
//
//   swift tools/make-h264-fixture.swift core/toolbox-core/tests/data/quadrants.h264
//
// Three 320x200 pictures of four flat quadrants, encoded with the system's
// H.264 encoder. The height is not a multiple of 16 on purpose, so the
// stream carries cropping. The file is a sequence of access units, each an
// Annex B byte stream behind a 4-byte big-endian length. The quadrant
// values are repeated in the test (rdp_h264.rs); change both together.
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

let width = 320, height = 200
// (Y, Cb, Cr) for top-left, top-right, bottom-left, bottom-right.
let pictures: [[(UInt8, UInt8, UInt8)]] = [
    [(200, 100, 180), (60, 200, 90), (120, 128, 128), (235, 60, 60)],
    [(200, 100, 180), (60, 200, 90), (120, 128, 128), (40, 128, 220)],
    [(90, 170, 110), (60, 200, 90), (120, 128, 128), (40, 128, 220)],
]

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: make-h264-fixture.swift <output>\n".data(using: .utf8)!)
    exit(2)
}
let outputPath = CommandLine.arguments[1]

func check(_ status: OSStatus, _ what: String) {
    guard status == noErr else {
        FileHandle.standardError.write("\(what) failed: \(status)\n".data(using: .utf8)!)
        exit(1)
    }
}

func makeBuffer(_ quadrants: [(UInt8, UInt8, UInt8)]) -> CVPixelBuffer {
    var buffer: CVPixelBuffer?
    let attributes = [kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary] as CFDictionary
    check(CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, attributes, &buffer),
          "CVPixelBufferCreate")
    let pixels = buffer!
    CVPixelBufferLockBaseAddress(pixels, [])
    let luma = CVPixelBufferGetBaseAddressOfPlane(pixels, 0)!.assumingMemoryBound(to: UInt8.self)
    let lumaStride = CVPixelBufferGetBytesPerRowOfPlane(pixels, 0)
    let chroma = CVPixelBufferGetBaseAddressOfPlane(pixels, 1)!.assumingMemoryBound(to: UInt8.self)
    let chromaStride = CVPixelBufferGetBytesPerRowOfPlane(pixels, 1)
    func quadrant(_ x: Int, _ y: Int, _ w: Int, _ h: Int) -> (UInt8, UInt8, UInt8) {
        quadrants[(y >= h / 2 ? 2 : 0) + (x >= w / 2 ? 1 : 0)]
    }
    for y in 0..<height {
        for x in 0..<width {
            luma[y * lumaStride + x] = quadrant(x, y, width, height).0
        }
    }
    for y in 0..<height / 2 {
        for x in 0..<width / 2 {
            let value = quadrant(x, y, width / 2, height / 2)
            chroma[y * chromaStride + 2 * x] = value.1
            chroma[y * chromaStride + 2 * x + 1] = value.2
        }
    }
    CVPixelBufferUnlockBaseAddress(pixels, [])
    return pixels
}

var session: VTCompressionSession?
check(VTCompressionSessionCreate(allocator: nil, width: Int32(width), height: Int32(height),
                                 codecType: kCMVideoCodecType_H264, encoderSpecification: nil,
                                 imageBufferAttributes: nil, compressedDataAllocator: nil,
                                 outputCallback: nil, refcon: nil, compressionSessionOut: &session),
      "VTCompressionSessionCreate")
let encoder = session!
VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_AllowFrameReordering, value: kCFBooleanFalse)
VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_ProfileLevel, value: kVTProfileLevel_H264_High_AutoLevel)
VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_MaxKeyFrameInterval, value: 1000 as CFNumber)
VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_AverageBitRate, value: 4_000_000 as CFNumber)
VTCompressionSessionPrepareToEncodeFrames(encoder)

let startCode: [UInt8] = [0, 0, 0, 1]
var output = Data()
var units = 0

func append(_ sample: CMSampleBuffer) {
    var unit = Data()
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
    let isKeyFrame = !((attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool) ?? false)
    if isKeyFrame, let format = CMSampleBufferGetFormatDescription(sample) {
        var count = 0
        check(CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                                 parameterSetSizeOut: nil, parameterSetCountOut: &count,
                                                                 nalUnitHeaderLengthOut: nil), "parameter set count")
        for index in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            check(CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, parameterSetIndex: index,
                                                                     parameterSetPointerOut: &pointer,
                                                                     parameterSetSizeOut: &size,
                                                                     parameterSetCountOut: nil,
                                                                     nalUnitHeaderLengthOut: nil), "parameter set")
            unit.append(contentsOf: startCode)
            unit.append(pointer!, count: size)
        }
    }
    let block = CMSampleBufferGetDataBuffer(sample)!
    var length = 0
    var bytes: UnsafeMutablePointer<CChar>?
    check(CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
                                      dataPointerOut: &bytes), "CMBlockBufferGetDataPointer")
    let data = Data(bytes: bytes!, count: length)
    var offset = 0
    while offset + 4 <= data.count {
        let size = data[offset..<offset + 4].reduce(0) { ($0 << 8) | Int($1) }
        offset += 4
        unit.append(contentsOf: startCode)
        unit.append(data[offset..<offset + size])
        offset += size
    }
    var header = UInt32(unit.count).bigEndian
    output.append(Data(bytes: &header, count: 4))
    output.append(unit)
    units += 1
}

for (index, quadrants) in pictures.enumerated() {
    let time = CMTime(value: CMTimeValue(index), timescale: 30)
    check(VTCompressionSessionEncodeFrame(encoder, imageBuffer: makeBuffer(quadrants), presentationTimeStamp: time,
                                          duration: .invalid, frameProperties: nil, infoFlagsOut: nil) { status, _, sample in
        check(status, "encode")
        if let sample { append(sample) }
    }, "VTCompressionSessionEncodeFrame")
}
check(VTCompressionSessionCompleteFrames(encoder, untilPresentationTimeStamp: .invalid), "complete")
VTCompressionSessionInvalidate(encoder)

guard units == pictures.count else {
    FileHandle.standardError.write("expected \(pictures.count) access units, got \(units)\n".data(using: .utf8)!)
    exit(1)
}
try output.write(to: URL(fileURLWithPath: outputPath))
print("wrote \(output.count) bytes, \(units) access units, to \(outputPath)")
