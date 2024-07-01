package me.jellysquid.mods.sodium.client.render.chunk.compile.pipeline;

import me.jellysquid.mods.sodium.client.model.color.ColorProvider;
import me.jellysquid.mods.sodium.client.model.color.ColorProviderRegistry;
import me.jellysquid.mods.sodium.client.model.light.LightMode;
import me.jellysquid.mods.sodium.client.model.light.LightPipeline;
import me.jellysquid.mods.sodium.client.model.light.LightPipelineProvider;
import me.jellysquid.mods.sodium.client.model.light.data.QuadLightData;
import me.jellysquid.mods.sodium.client.model.quad.BakedQuadView;
import me.jellysquid.mods.sodium.client.model.quad.properties.ModelQuadFacing;
import me.jellysquid.mods.sodium.client.model.quad.properties.ModelQuadOrientation;
import me.jellysquid.mods.sodium.client.render.chunk.compile.ChunkBuildBuffers;
import me.jellysquid.mods.sodium.client.render.chunk.compile.buffers.ChunkModelBuilder;
import me.jellysquid.mods.sodium.client.render.chunk.terrain.material.DefaultMaterials;
import me.jellysquid.mods.sodium.client.render.chunk.terrain.material.Material;
import me.jellysquid.mods.sodium.client.render.chunk.vertex.format.ChunkVertexEncoder;
import me.jellysquid.mods.sodium.client.util.DirectionUtil;
import net.caffeinemc.mods.sodium.api.util.ColorABGR;
import net.minecraft.block.BlockState;
import net.minecraft.client.MinecraftClient;
import net.minecraft.client.render.model.BakedModel;
import net.minecraft.client.render.model.BakedQuad;
import net.minecraft.client.texture.Sprite;
import net.minecraft.util.math.Direction;
import net.minecraft.util.math.Vec3d;
import net.minecraft.util.math.random.LocalRandom;
import net.minecraft.util.math.random.Random;

import java.util.Arrays;
import java.util.List;

public class BlockRenderer {
    private final Random random = new LocalRandom(42L);

    private final ColorProviderRegistry colorProviderRegistry;
    private final BlockOcclusionCache occlusionCache;

    private final QuadLightData quadLightData = new QuadLightData();

    private final LightPipelineProvider lighters;

    private final ChunkVertexEncoder.Vertex[] vertices = ChunkVertexEncoder.Vertex.uninitializedQuad();

    private final boolean useAmbientOcclusion;

    private final int[] quadColors = new int[4];

    public BlockRenderer(ColorProviderRegistry colorRegistry, LightPipelineProvider lighters) {
        this.colorProviderRegistry = colorRegistry;
        this.lighters = lighters;

        this.occlusionCache = new BlockOcclusionCache();
        this.useAmbientOcclusion = MinecraftClient.isAmbientOcclusionEnabled();
    }

    public void renderModel(BlockRenderContext ctx, ChunkBuildBuffers buffers) {
        var material = DefaultMaterials.forBlockState(ctx.state());
        var meshBuilder = buffers.get(material);

        ColorProvider<BlockState> colorizer = this.colorProviderRegistry.getColorProvider(ctx.state().getBlock());

        LightPipeline lighter = this.lighters.getLighter(this.getLightingMode(ctx.state(), ctx.model()));
        Vec3d renderOffset;

        if (ctx.state().hasModelOffset()) {
            renderOffset = ctx.state().getModelOffset(ctx.world(), ctx.pos());
        } else {
            renderOffset = Vec3d.ZERO;
        }

        // Pre-compute origin offsets for vertex position calculations
        final float originX = ctx.origin().x();
        final float originY = ctx.origin().y();
        final float originZ = ctx.origin().z();
        final float offsetX = (float) renderOffset.getX();
        final float offsetY = (float) renderOffset.getY();
        final float offsetZ = (float) renderOffset.getZ();

        // Iterate over all faces and render quads
        for (Direction face : DirectionUtil.ALL_DIRECTIONS) {
            List<BakedQuad> quads = this.getGeometry(ctx, face);

            if (!quads.isEmpty() && this.isFaceVisible(ctx, face)) {
                this.renderQuadList(ctx, material, lighter, colorizer, originX, originY, originZ, offsetX, offsetY, offsetZ, meshBuilder, quads, face);
            }
        }

        // Render all quads
        List<BakedQuad> all = this.getGeometry(ctx, null);

        if (!all.isEmpty()) {
            this.renderQuadList(ctx, material, lighter, colorizer, originX, originY, originZ, offsetX, offsetY, offsetZ, meshBuilder, all, null);
        }
    }

    private List<BakedQuad> getGeometry(BlockRenderContext ctx, Direction face) {
        var random = this.random;
        random.setSeed(ctx.seed());

        return ctx.model().getQuads(ctx.state(), face, random);
    }

    private boolean isFaceVisible(BlockRenderContext ctx, Direction face) {
        return this.occlusionCache.shouldDrawSide(ctx.state(), ctx.world(), ctx.pos(), face);
    }

    private void renderQuadList(BlockRenderContext ctx, Material material, LightPipeline lighter, ColorProvider<BlockState> colorizer,
                                float originX, float originY, float originZ, float offsetX, float offsetY, float offsetZ,
                                ChunkModelBuilder builder, List<BakedQuad> quads, Direction cullFace) {

        // Pre-compute sprite for potential optimizations
        Sprite sprite = null;

        // Iterate over quads and render
        for (BakedQuadView quad : quad) {
            final var lightData = this.getVertexLight(ctx, lighter, cullFace, quad);
            final var vertexColors = this.getVertexColors(ctx, colorizer, quad);

            // Optimize sprite addition: only add if sprite changes
            if (sprite != quad.getSprite()) {
                sprite = quad.getSprite();
                if (sprite != null) {
                    builder.addSprite(sprite);
                }
            }

            this.writeGeometry(ctx, builder, originX, originY, originZ, offsetX, offsetY, offsetZ, material, quad, vertexColors, lightData);
        }
    }

    private QuadLightData getVertexLight(BlockRenderContext ctx, LightPipeline lighter, Direction cullFace, BakedQuadView quad) {
        QuadLightData light = this.quadLightData;
        lighter.calculate(quad, ctx.pos(), light, cullFace, quad.getLightFace(), quad.hasShade());

        return light;
    }

    private int[] getVertexColors(BlockRenderContext ctx, ColorProvider<BlockState> colorProvider, BakedQuadView quad) {
        final int[] vertexColors = this.quadColors;

        if (colorProvider != null && quad.hasColor()) {
            colorProvider.getColors(ctx.world(), ctx.pos(), ctx.state(), quad, vertexColors);
        } else {
            Arrays.fill(vertexColors, 0xFFFFFFFF);
        }

        return vertexColors;
    }

    private void writeGeometry(BlockRenderContext ctx,
                               ChunkModelBuilder builder,
                               float originX, float originY, float originZ,
                               float offsetX, float offsetY, float offsetZ,
                               Material material,
                               BakedQuadView quad,
                               int[] colors,
                               QuadLightData light) {

        ModelQuadOrientation orientation = ModelQuadOrientation.orientByBrightness(light.br, light.lm);
        var vertices = this.vertices;

        ModelQuadFacing normalFace = quad.getNormalFace();

        for (int dstIndex = 0; dstIndex < 4; dstIndex++) {
            int srcIndex = orientation.getVertexIndex(dstIndex);

            var out = vertices[dstIndex];
            out.x = originX + quad.getX(srcIndex) + offsetX;
            out.y = originY + quad.getY(srcIndex) + offsetY;
            out.z = originZ + quad.getZ(srcIndex) + offsetZ;

            out.color = ColorABGR.withAlpha(colors != null ? colors[srcIndex] : 0xFFFFFFFF, light.br[srcIndex]);

            out.u = quad.getTexU(srcIndex);
            out.v = quad.getTexV(srcIndex);

            out.light = light.lm[srcIndex];
        }

        var vertexBuffer = builder.getVertexBuffer(normalFace);
        vertexBuffer.push(vertices, material);
    }

    private LightMode getLightingMode(BlockState state, BakedModel model) {
        if (this.useAmbientOcclusion && model.useAmbientOcclusion() && state.getLuminance() == 0) {
            return LightMode.SMOOTH;
        } else {
            return LightMode.FLAT;
        }
    }
                }
